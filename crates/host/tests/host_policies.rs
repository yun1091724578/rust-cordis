//! 第 3 步宿主策略完整验收套件：
//! 1. `loader.report` 将下游 `Pending` 精确关联到已知失败的上游行（含 apply 失败、预装载失败与热重载 Lost）；
//! 2. 行级 `required` / `optional` 启动策略（含 YAML/补丁/组继承/回写保真）与来源/完整性/持久化状态独立处置策略；
//! 3. 宿主管理调用入口（`call_iface`、`call_iface_stream`、`HostApi::ctx_call_iface`）的方法白名单与宿主绑定身份。

use cordis_abi::{
    Blob, ErrorPayload, Out, ERROR_FORMAT_V1, OP_APPLY, OP_CALLBACK, STATUS_ERR, STATUS_OK,
};
use cordis_core::{
    plugin_fn, with_provision, App, BoxFuture, Config, Ctx, Disposable, Dyn, ExternService,
    FiberState, Inject, LifecyclePolicy, MethodUnauthorized, Outcome, Plugin, Result, StreamSink,
    TraceRecord, TraceSink,
};
use cordis_host::loader::{
    load_with_source, reload_one_with_plugin, DiskPluginSource, LoadFailure, LoadedFile,
    LoaderConfig, PluginSource, PolicyPluginSource, ReloadOutcome, ReloadStatus,
};
use cordis_host::patch::{parse_entries, parse_patches, Entry};
use cordis_host::plan::PlanEntrySpec;
use cordis_host::source::{AllowedPlugin, PluginSourcePolicy, SourceAdmission};
use cordis_host::{
    apply_via_with_error_format, dispatches_len, host_api, method_allow_list_for,
    set_lifecycle_policy_for, set_method_allow_list_for, set_native_registry,
    set_startup_policy_for, staged_leaked, startup_policy_for, FailureCategory, FailureDisposition,
    MethodAllowList, NativeRegistry, StartupPolicy,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("cordis-host-policies-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).expect("write fixture file");
        path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn artifact_filename(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.dll")
    } else if cfg!(target_os = "macos") {
        format!("{stem}.dylib")
    } else {
        format!("{stem}.so")
    }
}

fn loaded_memory(name: &str, plugin: Arc<dyn Plugin>) -> LoadedFile {
    let path = PathBuf::from(format!("<policy-test:{name}>"));
    LoadedFile {
        plugin,
        source: path.clone(),
        staged: path,
        fingerprint: None,
    }
}

// ============================================================================
// 支柱 A：loader.report 把下游 Pending 关联到已知失败的上游行
// ============================================================================

struct ToggleAuthSource {
    fail_auth: Arc<AtomicBool>,
}

impl PluginSource for ToggleAuthSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "auth-plugin" => {
                let fail = self.fail_auth.clone();
                with_provision(
                    plugin_fn("auth-plugin", Inject::default(), move |ctx: Ctx, _| {
                        let fail = fail.clone();
                        Box::pin(async move {
                            if fail.load(Ordering::SeqCst) {
                                return Err("auth backend unreachable".into());
                            }
                            ctx.provide("auth", Dyn::new(String::from("token-ok")))?;
                            Ok(())
                        })
                    }),
                    vec!["auth".into()],
                )
            }
            "api-plugin" => plugin_fn("api-plugin", Inject::of(&["auth"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            other => return Err(format!("unknown plugin {other}")),
        };
        Ok(loaded_memory(&entry.name, plugin))
    }
}

#[tokio::test]
async fn pending_report_correlates_missing_key_to_failed_upstream_apply_and_clears_on_recovery() {
    let dir = TempDir::new("pending-apply");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: auth-row\n  name: auth-plugin\n  optional: true\n- id: api-row\n  name: api-plugin\n",
    );

    let fail_auth = Arc::new(AtomicBool::new(true));
    let source = Arc::new(ToggleAuthSource {
        fail_auth: fail_auth.clone(),
    });
    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        source,
    )
    .await
    .expect("可选上游行运行不可用时应降级启动成功");

    assert_eq!(state.mounted_labels(), vec!["api-row".to_string()]);
    assert_eq!(
        state.fiber_of("api-row").expect("api fiber").state(),
        FiberState::Pending
    );

    let report = state.report();
    let pending = report["pending"].as_array().expect("pending array");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["label"], "api-row");
    let missing = pending[0]["missing"].as_array().expect("missing array");
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0]["key"], "auth");
    assert_eq!(missing[0]["reason"], "missing");

    let upstream = missing[0]["failedUpstream"]
        .as_array()
        .expect("missing 项必须关联到失败上游行");
    assert_eq!(upstream.len(), 1);
    assert_eq!(upstream[0]["label"], "auth-row");
    assert_eq!(upstream[0]["plugin"], "auth-plugin");
    assert_eq!(upstream[0]["category"], "runtime_unavailable");
    assert_eq!(upstream[0]["startup"], "optional");
    assert_eq!(upstream[0]["degraded"], true);
    assert_eq!(upstream[0]["matchedKey"], "auth");
    assert!(upstream[0]["message"]
        .as_str()
        .unwrap_or("")
        .contains("auth backend unreachable"));

    // Typed API 与 JSON 报告一致。
    let correlated = state.correlated_pending_report();
    assert_eq!(correlated.len(), 1);
    assert_eq!(correlated[0].missing[0].failed_upstream.len(), 1);
    assert_eq!(
        correlated[0].missing[0].failed_upstream[0].label,
        "auth-row"
    );

    // 上游恢复后再次对账：下游自动激活，Pending 与 failed_rows 均清空。
    fail_auth.store(false, Ordering::SeqCst);
    let outcome = state.sync().await;
    assert_eq!(outcome.mounted, vec!["auth-row".to_string()]);
    state.fiber_of("api-row").expect("api").settle().await;
    assert_eq!(
        state.fiber_of("api-row").expect("api").state(),
        FiberState::Active
    );
    assert!(state.correlated_pending_report().is_empty());
    assert!(state.failed_rows().is_empty());
    let refreshed = state.report();
    assert!(refreshed["pending"].as_array().unwrap().is_empty());
    assert!(refreshed.get("failedRows").is_none());

    app.close().await;
}

struct PlanPreloadFailSource;

impl PluginSource for PlanPreloadFailSource {
    fn load(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
            "db-v1" => return Err("db driver binary unavailable".into()),
            "consumer-v1" => plugin_fn("consumer-v1", Inject::of(&["db@1"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            "consumer-v2" => plugin_fn("consumer-v2", Inject::of(&["db@2"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            "consumer-bare" => plugin_fn("consumer-bare", Inject::of(&["db"]), |_ctx, _| {
                Box::pin(async { Ok(()) })
            }),
            other => return Err(format!("unknown {other}")),
        };
        Ok(loaded_memory(&entry.name, plugin))
    }

    fn plan_entry(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
    ) -> std::result::Result<Option<PlanEntrySpec>, String> {
        if entry.name == "db-v1" {
            let mut provides = BTreeMap::new();
            provides.insert("db@1".into(), "1.2.0".into());
            return Ok(Some(PlanEntrySpec {
                id: "db-v1".into(),
                version: "1.2.0".into(),
                provides,
                requires: BTreeMap::new(),
            }));
        }
        Ok(None)
    }
}

#[tokio::test]
async fn pending_report_correlates_preload_failure_via_plan_entry_and_respects_major_version() {
    let dir = TempDir::new("pending-plan");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: db-v1\n  name: db-v1\n  startup: optional\n- id: c1\n  name: consumer-v1\n- id: c2\n  name: consumer-v2\n- id: c-bare\n  name: consumer-bare\n",
    );

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(PlanPreloadFailSource),
    )
    .await
    .expect("可选上游 pre-load 失败时应降级启动");

    let report = state.report();
    let pending = report["pending"].as_array().expect("pending");
    let find = |label: &str| {
        pending
            .iter()
            .find(|row| row["label"] == label)
            .expect("missing pending row")
    };

    // db@1 精确匹配失败上游 db-v1。
    let c1_missing = &find("c1")["missing"][0];
    assert_eq!(c1_missing["key"], "db@1");
    assert_eq!(c1_missing["failedUpstream"][0]["label"], "db-v1");
    assert_eq!(c1_missing["failedUpstream"][0]["matchedKey"], "db@1");

    // 裸名 db 匹配同 base 的失败上游 db@1。
    let bare_missing = &find("c-bare")["missing"][0];
    assert_eq!(bare_missing["key"], "db");
    assert_eq!(bare_missing["failedUpstream"][0]["label"], "db-v1");

    // db@2 主版本冲突：绝不能把 db@1 误关联到 db@2。
    let c2_missing = &find("c2")["missing"][0];
    assert_eq!(c2_missing["key"], "db@2");
    assert!(
        c2_missing.get("failedUpstream").is_none(),
        "db@2 不应误关联到 db@1: {c2_missing}"
    );

    app.close().await;
}

#[tokio::test]
async fn pending_report_tracks_lost_reload_provider_and_ignores_rolled_back_provider() {
    struct InitialSource;
    impl PluginSource for InitialSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
                "prov" => with_provision(
                    plugin_fn("prov", Inject::default(), |ctx: Ctx, _| {
                        Box::pin(async move {
                            ctx.provide("svc", Dyn::new(String::from("v1")))?;
                            Ok(())
                        })
                    }),
                    vec!["svc".into()],
                ),
                "cons" => plugin_fn("cons", Inject::of(&["svc"]), |_ctx, _| {
                    Box::pin(async { Ok(()) })
                }),
                other => return Err(format!("unknown {other}")),
            };
            Ok(loaded_memory(&entry.name, plugin))
        }
    }

    let dir = TempDir::new("pending-reload-lost");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: prov\n  name: prov\n- id: cons\n  name: cons\n",
    );

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(InitialSource),
    )
    .await
    .expect("initial load");
    state.fiber_of("cons").unwrap().settle().await;
    assert_eq!(state.fiber_of("cons").unwrap().state(), FiberState::Active);

    // 1) 新版 apply 失败，回滚到旧版成功 ⇒ 旧版仍然 Active，不产生失败上游关联。
    let bad_new = with_provision(
        plugin_fn("prov", Inject::default(), |_ctx, _| {
            Box::pin(async { Err("v2 boom".into()) })
        }),
        vec!["svc".into()],
    );
    let rollback_report = reload_one_with_plugin(&app.root, &state, "prov", bad_new).await;
    assert_eq!(rollback_report.status, ReloadStatus::RolledBack);
    state.fiber_of("cons").unwrap().settle().await;
    assert_eq!(state.fiber_of("cons").unwrap().state(), FiberState::Active);
    assert!(state.failed_rows().is_empty());

    // 2) 先换上一个只成功一次（回滚必败）的插件，再换一个必败插件 ⇒ 触发 ReloadStatus::Lost。
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_clone = attempts.clone();
    let one_shot = with_provision(
        plugin_fn("prov", Inject::default(), move |ctx: Ctx, _| {
            let n = attempts_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n > 0 {
                    return Err("rollback failed too".into());
                }
                ctx.provide("svc", Dyn::new(String::from("v1.5")))?;
                Ok(())
            })
        }),
        vec!["svc".into()],
    );
    let ok_swap = reload_one_with_plugin(&app.root, &state, "prov", one_shot).await;
    assert_eq!(ok_swap.status, ReloadStatus::Active);

    let bad_v3 = with_provision(
        plugin_fn("prov", Inject::default(), |_ctx, _| {
            Box::pin(async { Err("v3 boom".into()) })
        }),
        vec!["svc".into()],
    );
    let lost_report = reload_one_with_plugin(&app.root, &state, "prov", bad_v3).await;
    assert_eq!(lost_report.status, ReloadStatus::Lost);
    state.fiber_of("cons").unwrap().settle().await;
    assert_eq!(state.fiber_of("cons").unwrap().state(), FiberState::Pending);

    let report = state.report();
    let pending = report["pending"].as_array().expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["label"], "cons");
    assert_eq!(
        pending[0]["missing"][0]["failedUpstream"][0]["label"],
        "prov"
    );

    app.close().await;
}

// ============================================================================
// 支柱 B：行级 required / optional + 来源 / 完整性 / 持久化状态独立处置策略
// ============================================================================

struct ClassifiedTestSource {
    outcomes: Arc<Mutex<BTreeMap<String, std::result::Result<(), LoadFailure>>>>,
}

impl ClassifiedTestSource {
    fn new(
        map: impl IntoIterator<Item = (&'static str, std::result::Result<(), LoadFailure>)>,
    ) -> Self {
        let mut outcomes = BTreeMap::new();
        for (k, v) in map {
            outcomes.insert(k.to_string(), v);
        }
        Self {
            outcomes: Arc::new(Mutex::new(outcomes)),
        }
    }
}

impl PluginSource for ClassifiedTestSource {
    fn load(
        &self,
        entry: &Entry,
        search: &[PathBuf],
        revision: u64,
    ) -> std::result::Result<LoadedFile, String> {
        self.load_classified(entry, search, revision, &SourceAdmission::fallback())
            .map_err(|err| err.message)
    }

    fn load_classified(
        &self,
        entry: &Entry,
        _search: &[PathBuf],
        _revision: u64,
        _admission: &SourceAdmission,
    ) -> std::result::Result<LoadedFile, LoadFailure> {
        let decision = self
            .outcomes
            .lock()
            .unwrap()
            .get(&entry.name)
            .cloned()
            .unwrap_or(Ok(()));
        match decision {
            Ok(()) => {
                let plugin_name: &'static str = Box::leak(entry.name.clone().into_boxed_str());
                let plugin = plugin_fn(plugin_name, Inject::default(), |_ctx, _| {
                    Box::pin(async { Ok(()) })
                });
                Ok(loaded_memory(&entry.name, plugin))
            }
            Err(failure) => Err(failure),
        }
    }
}

#[tokio::test]
async fn optional_row_degrades_on_runtime_unavailable_with_group_inheritance_patch_and_persist() {
    // 1) YAML 非法/冲突声明必须 fail loud。
    let conflict = parse_entries("- id: a\n  name: p\n  required: true\n  optional: true\n")
        .expect_err("冲突声明必须报错");
    assert!(conflict.contains("冲突"), "{conflict}");
    let bad_type =
        parse_entries("- id: a\n  name: p\n  required: \"false\"\n").expect_err("错误类型必须报错");
    assert!(bad_type.contains("required"), "{bad_type}");
    let bad_startup = parse_entries("- id: a\n  name: p\n  startup: maybe\n")
        .expect_err("非法 startup 枚举必须报错");
    assert!(bad_startup.contains("startup"), "{bad_startup}");
    let bad_patch = parse_patches("- id: a\n  optional: 123\n").expect_err("非法 patch 必须报错");
    assert!(bad_patch.contains("optional"), "{bad_patch}");

    // 2) 组继承 + 子行 required 覆盖 + patch 覆盖为 optional + 持久化回写保真。
    let dir = TempDir::new("row-startup-yaml");
    let main_path = dir.write(
        "cordis.yml",
        "\
- id: core
  name: core
  config:
    count: 1
- id: opt-direct
  name: opt-direct
  optional: true
- id: patched-opt
  name: patched-opt
- id: opt-group
  name: grp
  group: true
  required: false
  config:
    - id: grp-opt
      name: grp-opt
    - id: grp-req
      name: grp-req
      required: true
",
    );
    let patch_path = dir.write(
        "cordis.patch.yml",
        "- id: patched-opt\n  startup: optional\n",
    );

    let source = Arc::new(ClassifiedTestSource::new([
        ("core", Ok(())),
        ("opt-direct", Err(LoadFailure::runtime("binary missing"))),
        ("patched-opt", Err(LoadFailure::runtime("service offline"))),
        ("grp-opt", Err(LoadFailure::runtime("optional child down"))),
        ("grp-req", Ok(())),
    ]));

    let app = App::new();
    let mut cfg = LoaderConfig::new(main_path.to_string_lossy().into_owned());
    cfg.patches = vec![patch_path.to_string_lossy().into_owned()];
    cfg.strict = true;

    let state = load_with_source(app.root.clone(), cfg.clone(), source.clone())
        .await
        .expect("所有可选行运行不可用时在 strict=true 下仍应降级成功");

    assert_eq!(
        state.mounted_labels(),
        vec!["core".to_string(), "grp-req".to_string()]
    );
    let report = state.report();
    assert!(
        report["failed"].as_array().unwrap().is_empty(),
        "仅可选行降级时 fatal failed 应为空: {report}"
    );
    assert_eq!(report["degraded"].as_array().unwrap().len(), 3);
    assert_eq!(state.degraded_rows().len(), 3);

    // 触发 core 配置回写：持久化到 cordis.yml 后不得丢失 opt-direct / opt-group / grp-req 的 startup 声明！
    let core_fiber = state.fiber_of("core").expect("core fiber");
    core_fiber
        .update(json!({ "count": 2 }))
        .await
        .expect("update core");
    let persisted = std::fs::read_to_string(&main_path).expect("read persisted cordis.yml");
    assert!(
        persisted.contains("startup: optional"),
        "回写主配置必须保留 optional 声明: {persisted}"
    );
    assert!(
        persisted.contains("startup: required"),
        "回写主配置必须保留显式 required 子行覆盖: {persisted}"
    );
    // 再次用回写后的磁盘配置装载，行为应完全一致。
    app.close().await;

    let app2 = App::new();
    let state2 = load_with_source(app2.root.clone(), cfg.clone(), source.clone())
        .await
        .expect("回写后的配置重新装载仍应保持行级 optional 语义");
    assert_eq!(
        state2.mounted_labels(),
        vec!["core".to_string(), "grp-req".to_string()]
    );
    app2.close().await;

    // 3) 子行显式 required: true 失败 ⇒ 即使父组是 optional，仍阻断启动！
    source.outcomes.lock().unwrap().insert(
        "grp-req".into(),
        Err(LoadFailure::runtime("required child failed")),
    );
    let app3 = App::new();
    let err = load_with_source(app3.root.clone(), cfg, source)
        .await
        .expect_err("显式覆盖为 required 的子行失败必须阻断启动");
    assert!(
        err.to_string().contains("grp-req"),
        "错误信息应指向失败的必需子行: {err}"
    );
    app3.close().await;
}

#[tokio::test]
async fn optional_row_does_not_swallow_source_or_integrity_errors_by_default() {
    let dir = TempDir::new("opt-fail-closed");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: core\n  name: core\n- id: opt\n  name: opt\n  startup: optional\n",
    );

    // 来源错误：即使行声明为 optional，默认仍 fail-closed。
    let app1 = App::new();
    let src_err = load_with_source(
        app1.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt", Err(LoadFailure::source("not in allow-list"))),
        ])),
    )
    .await
    .expect_err("optional 行不得静默吞掉 Source 错误");
    assert!(src_err.to_string().contains("category=source"), "{src_err}");
    app1.close().await;

    // 完整性错误：即使行声明为 optional，默认仍 fail-closed。
    let app2 = App::new();
    let int_err = load_with_source(
        app2.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt", Err(LoadFailure::integrity("SHA256 mismatch"))),
        ])),
    )
    .await
    .expect_err("optional 行不得静默吞掉 Integrity 错误");
    assert!(
        int_err.to_string().contains("category=integrity"),
        "{int_err}"
    );
    app2.close().await;
}

#[tokio::test]
async fn startup_policy_controls_source_integrity_and_persisted_state_independently() {
    let dir = TempDir::new("independent-policies");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: core\n  name: core\n- id: opt-src\n  name: opt-src\n  optional: true\n- id: opt-int\n  name: opt-int\n  optional: true\n",
    );
    let bad_patch = dir.write("broken.patch.yml", "not: [valid: yaml");

    // 1) 仅允许 Source 降级：Integrity 与 PersistedState 仍必须 fail-closed（即使 strict=false）。
    let app1 = App::new();
    let policy1 = StartupPolicy::new().with_source(FailureDisposition::Degrade);
    set_startup_policy_for(&app1.root, Some(policy1.clone()));
    assert_eq!(startup_policy_for(&app1.root), Some(policy1));

    let mut cfg1 = LoaderConfig::new(cfg_path.to_string_lossy().into_owned());
    cfg1.strict = false; // 安装 StartupPolicy 后，三类敏感错误服从各自开关而非 strict=false
    let err = load_with_source(
        app1.root.clone(),
        cfg1.clone(),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Err(LoadFailure::source("unlisted"))),
            ("opt-int", Err(LoadFailure::integrity("sha256 bad"))),
        ])),
    )
    .await
    .expect_err("source=Degrade 不得放行 integrity=Fail");
    assert!(err.to_string().contains("category=integrity"), "{err}");

    // 只发生 Source 错误时，source=Degrade 放行并记录在 report["degraded"]。
    app1.close().await;
    let app1_ok = App::new();
    set_startup_policy_for(
        &app1_ok.root,
        Some(StartupPolicy::new().with_source(FailureDisposition::Degrade)),
    );
    let state1 = load_with_source(
        app1_ok.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Err(LoadFailure::source("unlisted"))),
            ("opt-int", Ok(())),
        ])),
    )
    .await
    .expect("source=Degrade 应降级来源错误");
    assert_eq!(
        state1.mounted_labels(),
        vec!["core".to_string(), "opt-int".to_string()]
    );
    let rows = state1.failed_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].category, FailureCategory::Source);
    assert!(rows[0].degraded);
    app1_ok.close().await;

    // 2) 仅允许 Integrity 降级：Source 仍 fail-closed。
    let app2 = App::new();
    set_startup_policy_for(
        &app2.root,
        Some(StartupPolicy::new().with_integrity(FailureDisposition::Degrade)),
    );
    let state2 = load_with_source(
        app2.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Ok(())),
            ("opt-int", Err(LoadFailure::integrity("sha256 bad"))),
        ])),
    )
    .await
    .expect("integrity=Degrade 应降级完整性错误");
    assert_eq!(state2.failed_rows()[0].category, FailureCategory::Integrity);
    assert!(state2.failed_rows()[0].degraded);
    app2.close().await;

    let app2_deny = App::new();
    set_startup_policy_for(
        &app2_deny.root,
        Some(StartupPolicy::new().with_integrity(FailureDisposition::Degrade)),
    );
    let err2 = load_with_source(
        app2_deny.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Err(LoadFailure::source("unlisted"))),
            ("opt-int", Ok(())),
        ])),
    )
    .await
    .expect_err("integrity=Degrade 不得放行 source=Fail");
    assert!(err2.to_string().contains("category=source"), "{err2}");
    app2_deny.close().await;

    // 3) PersistedState 独立处置：损坏 patch 默认阻断；设 Degrade 后跳过损坏 patch 并上报 persistedStateErrors。
    let app3_fail = App::new();
    let mut cfg3 = LoaderConfig::new(cfg_path.to_string_lossy().into_owned());
    cfg3.patches = vec![bad_patch.to_string_lossy().into_owned()];
    let err3 = load_with_source(
        app3_fail.root.clone(),
        cfg3.clone(),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Ok(())),
            ("opt-int", Ok(())),
        ])),
    )
    .await
    .expect_err("默认 persisted_state=Fail 必须阻断损坏 patch");
    assert!(err3.to_string().contains("patch"), "{err3}");
    app3_fail.close().await;

    let app3 = App::new();
    set_startup_policy_for(
        &app3.root,
        Some(StartupPolicy::new().with_persisted_state(FailureDisposition::Degrade)),
    );
    let state3 = load_with_source(
        app3.root.clone(),
        cfg3,
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Ok(())),
            ("opt-int", Ok(())),
        ])),
    )
    .await
    .expect("persisted_state=Degrade 应跳过损坏 patch 并启动主配置");
    assert_eq!(state3.mounted_labels().len(), 3);
    let persisted_errs = state3.persisted_state_errors();
    assert_eq!(persisted_errs.len(), 1);
    assert_eq!(persisted_errs[0].category, FailureCategory::PersistedState);
    assert!(persisted_errs[0].degraded);
    assert_eq!(
        state3.report()["persistedStateErrors"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // 运行期回写基线冲突也必须进 persistedStateErrors 并在 reload 恢复后清理。
    std::fs::write(
        &cfg_path,
        "- id: core\n  name: core\n  config:\n    ext: 99\n",
    )
    .unwrap();
    state3
        .fiber_of("core")
        .unwrap()
        .update(json!({ "local": 1 }))
        .await
        .expect("update triggers writeback conflict");
    assert!(
        state3
            .persisted_state_errors()
            .iter()
            .any(|err| err.label == "core" && err.category == FailureCategory::PersistedState),
        "运行期回写冲突必须进入 persisted_state_errors: {:?}",
        state3.persisted_state_errors()
    );
    app3.close().await;

    // 3b) 主配置本身损坏：默认 fail-closed；设 persisted_state=Degrade 后以空树启动并上报 persistedStateErrors，
    //     且不得覆写磁盘上尚未修复的损坏主配置文件。
    let bad_main = dir.write("broken-main.yml", "not: [valid: yaml");
    let app3b_fail = App::new();
    load_with_source(
        app3b_fail.root.clone(),
        LoaderConfig::new(bad_main.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([])),
    )
    .await
    .expect_err("默认 persisted_state=Fail 必须阻断损坏主配置");
    app3b_fail.close().await;

    let app3b_deg = App::new();
    set_startup_policy_for(
        &app3b_deg.root,
        Some(StartupPolicy::new().with_persisted_state(FailureDisposition::Degrade)),
    );
    let state3b = load_with_source(
        app3b_deg.root.clone(),
        LoaderConfig::new(bad_main.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([])),
    )
    .await
    .expect("persisted_state=Degrade 应降级损坏主配置");
    assert!(state3b.mounted_labels().is_empty());
    assert_eq!(state3b.persisted_state_errors().len(), 1);
    assert_eq!(
        std::fs::read_to_string(&bad_main).unwrap(),
        "not: [valid: yaml",
        "降级后不得改写磁盘上损坏的原始主配置文件"
    );
    app3b_deg.close().await;

    // 4) 宿主程序化行覆盖：强制把 YAML 中的 optional 行升格为 Required。
    let cfg_override = dir.write(
        "cordis-override.yml",
        "- id: core\n  name: core\n- id: opt-src\n  name: opt-src\n  optional: true\n- id: opt-int\n  name: opt-int\n  optional: true\n",
    );
    let app4 = App::new();
    set_startup_policy_for(
        &app4.root,
        Some(StartupPolicy::new().with_required_row("opt-src")),
    );
    let err4 = load_with_source(
        app4.root.clone(),
        LoaderConfig::new(cfg_override.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("core", Ok(())),
            ("opt-src", Err(LoadFailure::runtime("runtime down"))),
            ("opt-int", Ok(())),
        ])),
    )
    .await
    .expect_err("宿主程序化 with_required_row 必须覆盖 YAML 的 optional: true");
    assert!(err4.to_string().contains("opt-src"), "{err4}");
    app4.close().await;
}

#[tokio::test]
async fn real_disk_plugin_source_distinguishes_source_and_integrity_for_optional_rows() {
    const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    const SHA_OTHER: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    let dir = TempDir::new("disk-source-integrity");
    let plugin_file = dir.path().join(artifact_filename("optplug"));
    std::fs::write(&plugin_file, b"abc").unwrap();
    let manifest_file = cordis_host::source::manifest_path(&plugin_file);
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: opt\n  name: optplug\n  optional: true\n",
    );

    // 场景 1：artifact 与 manifest SHA256 一致，但不在 allow-list ⇒ 精确归类为 Source。
    std::fs::write(
        &manifest_file,
        json!({
            "id": "optplug",
            "version": "0.1.0",
            "source_revision": "rev1",
            "abi_version": 1,
            "sha256": SHA_ABC,
        })
        .to_string(),
    )
    .unwrap();
    let strict_policy = Arc::new(PluginSourcePolicy::new(vec![AllowedPlugin {
        id: "optplug".into(),
        version: "0.1.0".into(),
        source_revision: "rev1".into(),
        abi_version: 1,
        sha256: SHA_OTHER.into(),
    }]));
    let source = Arc::new(PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        strict_policy.clone(),
    ));

    // 默认 fail-closed：即使 optional: true，也不放行 Source 拒绝。
    let app = App::new();
    let err = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        source.clone(),
    )
    .await
    .expect_err("真实 DiskPluginSource 的 allow-list 拒绝不得被 optional 吞掉");
    assert!(err.to_string().contains("category=source"), "{err}");
    assert_eq!(staged_leaked(), 0);
    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .all(|e| !e.file_name().to_string_lossy().contains(".r")),
        "来源准入拒绝发生在暂存前，目录内不得产生 .rN 暂存副本"
    );
    app.close().await;

    // 显式 source=Degrade（integrity 保持 Fail）⇒ 允许降级并精确标记 category=source。
    let app_src_deg = App::new();
    set_startup_policy_for(
        &app_src_deg.root,
        Some(StartupPolicy::new().with_source(FailureDisposition::Degrade)),
    );
    let state = load_with_source(
        app_src_deg.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        source.clone(),
    )
    .await
    .expect("source=Degrade 应降级真实 DiskPluginSource 的 Source 错误");
    assert!(state.mounted_labels().is_empty());
    assert_eq!(state.failed_rows()[0].category, FailureCategory::Source);
    assert!(state.failed_rows()[0].degraded);
    app_src_deg.close().await;

    // 场景 2：篡改 artifact 字节导致 SHA256 与 manifest 不一致 ⇒ 精确归类为 Integrity。
    std::fs::write(&plugin_file, b"tampered-bytes").unwrap();
    let app2 = App::new();
    // 即使 source=Degrade，Integrity 仍必须 fail-closed！
    set_startup_policy_for(
        &app2.root,
        Some(StartupPolicy::new().with_source(FailureDisposition::Degrade)),
    );
    let err2 = load_with_source(
        app2.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        source.clone(),
    )
    .await
    .expect_err("篡改 artifact 归类为 Integrity，不得被 source=Degrade 放行");
    assert!(err2.to_string().contains("category=integrity"), "{err2}");
    assert_eq!(staged_leaked(), 0);
    app2.close().await;

    // 显式 integrity=Degrade ⇒ 允许降级并精确标记 category=integrity。
    let app2_deg = App::new();
    set_startup_policy_for(
        &app2_deg.root,
        Some(StartupPolicy::new().with_integrity(FailureDisposition::Degrade)),
    );
    let state2 = load_with_source(
        app2_deg.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        source,
    )
    .await
    .expect("integrity=Degrade 应降级篡改错误");
    assert!(state2.mounted_labels().is_empty());
    assert_eq!(state2.failed_rows()[0].category, FailureCategory::Integrity);
    assert!(state2.failed_rows()[0].degraded);
    assert_eq!(staged_leaked(), 0);
    app2_deg.close().await;
}

// ============================================================================
// 支柱 C：宿主管理调用入口的方法白名单 + 宿主绑定身份
// ============================================================================

struct KvService {
    reads: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
    streams: Arc<AtomicUsize>,
}

impl ExternService for KvService {
    fn service(&self) -> &str {
        "kv"
    }

    fn version(&self) -> u32 {
        1
    }

    fn methods(&self) -> Vec<String> {
        vec!["read".into(), "write".into()]
    }

    fn call(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
    ) -> BoxFuture<Result<Config>> {
        match method.as_str() {
            "read" => {
                self.reads.fetch_add(1, Ordering::SeqCst);
            }
            "write" => {
                self.writes.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        }
        Box::pin(async move { Ok(json!({ "ok": method })) })
    }

    fn call_stream(
        &self,
        _caller_meta: Option<Config>,
        method: String,
        _args: Config,
        sink: StreamSink,
    ) -> BoxFuture<Result<()>> {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            sink.push(json!({ "stream": method }));
            sink.close();
            Ok(())
        })
    }
}

#[tokio::test]
async fn method_allow_list_enforces_host_bound_row_identity_without_trace_sink() {
    struct SamePluginTwoRowsSource {
        reads: Arc<AtomicUsize>,
        writes: Arc<AtomicUsize>,
        streams: Arc<AtomicUsize>,
        apply_read_ok: Arc<AtomicBool>,
        apply_write_denied: Arc<AtomicBool>,
        child_write_denied: Arc<AtomicBool>,
    }

    impl PluginSource for SamePluginTwoRowsSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
                "kv-provider" => {
                    let reads = self.reads.clone();
                    let writes = self.writes.clone();
                    let streams = self.streams.clone();
                    plugin_fn("kv-provider", Inject::default(), move |ctx: Ctx, _| {
                        let reads = reads.clone();
                        let writes = writes.clone();
                        let streams = streams.clone();
                        Box::pin(async move {
                            ctx.provide_iface(
                                "kv",
                                Arc::new(KvService {
                                    reads,
                                    writes,
                                    streams,
                                }),
                            )?;
                            Ok(())
                        })
                    })
                }
                "worker" => {
                    let apply_read_ok = self.apply_read_ok.clone();
                    let apply_write_denied = self.apply_write_denied.clone();
                    let child_write_denied = self.child_write_denied.clone();
                    plugin_fn("worker", Inject::of(&["kv"]), move |ctx: Ctx, _| {
                        let apply_read_ok = apply_read_ok.clone();
                        let apply_write_denied = apply_write_denied.clone();
                        let child_write_denied = child_write_denied.clone();
                        Box::pin(async move {
                            let id = ctx.caller_identity();
                            // 在 apply 首跳即可稳定看到由装载器绑定的 row_label（无竞态）。
                            if id.row_label.as_deref() == Some("worker-ro") {
                                if ctx.call_iface("kv", 1, "read", Config::Null).await.is_ok() {
                                    apply_read_ok.store(true, Ordering::SeqCst);
                                }
                                let err = ctx
                                    .call_iface("kv", 1, "write", Config::Null)
                                    .await
                                    .expect_err("worker-ro 在 apply 中也不得调用 kv.write");
                                if err.downcast_ref::<MethodUnauthorized>().is_some() {
                                    apply_write_denied.store(true, Ordering::SeqCst);
                                }
                            } else if id.row_label.as_deref() == Some("worker-rw") {
                                // 子插件不得继承父行的 worker-rw 授权。
                                let child = plugin_fn(
                                    "rogue-child",
                                    Inject::of(&["kv"]),
                                    move |child_ctx: Ctx, _| {
                                        let child_write_denied = child_write_denied.clone();
                                        Box::pin(async move {
                                            let err = child_ctx
                                                .call_iface("kv", 1, "write", Config::Null)
                                                .await
                                                .expect_err("子 fiber 不得继承父行白名单");
                                            if err.downcast_ref::<MethodUnauthorized>().is_some() {
                                                child_write_denied.store(true, Ordering::SeqCst);
                                            }
                                            Ok(())
                                        })
                                    },
                                );
                                let f = ctx.plugin(child, Config::Null);
                                f.settle().await;
                            }
                            Ok(())
                        })
                    })
                }
                other => return Err(format!("unknown {other}")),
            };
            Ok(loaded_memory(&entry.name, plugin))
        }
    }

    let dir = TempDir::new("allow-list-rows");
    let cfg_path = dir.write(
        "cordis.yml",
        "- id: kv\n  name: kv-provider\n- id: worker-ro\n  name: worker\n- id: worker-rw\n  name: worker\n",
    );

    let app = App::new();
    assert!(
        !app.store.has_trace_sink(),
        "方法白名单必须完全不依赖 TraceSink"
    );

    let allow = MethodAllowList::new()
        .allow_row_method("worker-ro", "kv", "read")
        .allow_row_methods("worker-rw", "kv", ["read", "write"]);
    set_method_allow_list_for(&app.root, Some(allow.clone()));
    assert_eq!(method_allow_list_for(&app.root), Some(allow));

    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let streams = Arc::new(AtomicUsize::new(0));
    let apply_read_ok = Arc::new(AtomicBool::new(false));
    let apply_write_denied = Arc::new(AtomicBool::new(false));
    let child_write_denied = Arc::new(AtomicBool::new(false));

    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(SamePluginTwoRowsSource {
            reads: reads.clone(),
            writes: writes.clone(),
            streams: streams.clone(),
            apply_read_ok: apply_read_ok.clone(),
            apply_write_denied: apply_write_denied.clone(),
            child_write_denied: child_write_denied.clone(),
        }),
    )
    .await
    .expect("load rows");

    state.fiber_of("worker-ro").unwrap().settle().await;
    state.fiber_of("worker-rw").unwrap().settle().await;
    assert!(
        apply_read_ok.load(Ordering::SeqCst),
        "worker-ro apply 首跳 read 应成功"
    );
    assert!(
        apply_write_denied.load(Ordering::SeqCst),
        "worker-ro apply 首跳 write 应被拒"
    );
    assert!(
        child_write_denied.load(Ordering::SeqCst),
        "子 fiber write 必须被拒"
    );
    assert_eq!(
        writes.load(Ordering::SeqCst),
        0,
        "被拒调用不得执行提供者 write"
    );

    // 同一插件的两个行拥有不同权限。
    let ro_ctx = state.fiber_of("worker-ro").unwrap().ctx();
    let rw_ctx = state.fiber_of("worker-rw").unwrap().ctx();

    assert!(ro_ctx
        .call_iface("kv", 1, "read", Config::Null)
        .await
        .is_ok());
    assert!(ro_ctx
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_err());
    // 派生 isolate/intercept 保留绑定身份（既不绕过也不丢失）。
    let ro_derived = ro_ctx.intercept("kv", json!({ "from": "ro" }));
    assert!(ro_derived
        .call_iface("kv", 1, "read", Config::Null)
        .await
        .is_ok());
    assert!(ro_derived
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_err());

    // 流式调用同样在提供者执行前拦截。
    assert!(ro_ctx
        .call_iface_stream("kv", 1, "write", Config::Null)
        .is_err());
    assert_eq!(
        streams.load(Ordering::SeqCst),
        0,
        "被拒流式调用不得进入提供者"
    );
    let mut s = ro_ctx
        .call_iface_stream("kv", 1, "read", Config::Null)
        .expect("read 流应放行");
    assert_eq!(
        s.next().await.expect("stream chunk"),
        Some(json!({ "stream": "read" }))
    );
    assert_eq!(streams.load(Ordering::SeqCst), 1);

    // worker-rw 拥有 write 权限。
    assert!(rw_ctx
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_ok());
    assert_eq!(writes.load(Ordering::SeqCst), 1);

    // 根上下文默认放行；切到 deny_all 后根上下文也被拒。
    assert!(app
        .root
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_ok());
    assert_eq!(writes.load(Ordering::SeqCst), 2);
    set_method_allow_list_for(&app.root, Some(MethodAllowList::deny_all()));
    assert!(app
        .root
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_err());
    assert_eq!(writes.load(Ordering::SeqCst), 2);

    // 清除白名单恢复无限制。
    set_method_allow_list_for(&app.root, None);
    assert!(method_allow_list_for(&app.root).is_none());
    assert!(ro_ctx
        .call_iface("kv", 1, "write", Config::Null)
        .await
        .is_ok());
    assert_eq!(writes.load(Ordering::SeqCst), 3);

    // 不同 App 的策略完全隔离，互不泄漏。
    let app_other = App::new();
    set_method_allow_list_for(&app.root, Some(MethodAllowList::deny_all()));
    assert!(method_allow_list_for(&app_other.root).is_none());
    assert!(!app_other.store.has_call_auth_hook());
    app_other.close().await;

    app.close().await;
}

struct RecordingTraceSink(Mutex<Vec<TraceRecord>>);

impl TraceSink for RecordingTraceSink {
    fn record(&self, record: TraceRecord) {
        self.0.lock().unwrap().push(record);
    }
}

#[tokio::test]
async fn t2_host_ctx_call_iface_enforces_method_allow_list_and_returns_structured_unauthorized() {
    static PROVIDER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static ALLOWED_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static DENIED_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn provider_entry(
        handle: u64,
        op: u32,
        input: Blob,
        _reserved: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                let mut buf = vec![0u8; 512];
                let mut host_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let provide = host_api().ctx_provide_iface.expect("ctx_provide_iface");
                let status = provide(
                    handle,
                    Blob::new(b"vault"),
                    1,
                    Blob::new(br#"["read","secret"]"#),
                    1,
                    &mut host_out,
                );
                if status != STATUS_OK {
                    return STATUS_ERR;
                }
                STATUS_OK
            }
            OP_CALLBACK => {
                PROVIDER_CALLS.fetch_add(1, Ordering::SeqCst);
                let req: Value = serde_json::from_slice(input.as_bytes()).unwrap_or(Value::Null);
                let reply = json!({ "ret": { "called": req["method"] } }).to_string();
                let bytes = reply.as_bytes();
                let out_ref = &mut *out;
                out_ref.len = bytes.len();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ref.ptr, bytes.len());
                STATUS_OK
            }
            _ => STATUS_OK,
        }
    }

    unsafe extern "C" fn caller_allowed_entry(
        handle: u64,
        op: u32,
        _name: Blob,
        _payload: Blob,
        _out: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            ALLOWED_HANDLE.store(handle, Ordering::SeqCst);
        }
        STATUS_OK
    }

    unsafe extern "C" fn caller_denied_entry(
        handle: u64,
        op: u32,
        _name: Blob,
        _payload: Blob,
        _out: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            DENIED_HANDLE.store(handle, Ordering::SeqCst);
        }
        STATUS_OK
    }

    unsafe fn raw_call_iface(handle: u64, service: &str, method: &str) -> (i32, String) {
        let call = host_api().ctx_call_iface.expect("ctx_call_iface");
        let mut buf = vec![0u8; 4096];
        let mut out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let status = call(
            handle,
            Blob::new(service.as_bytes()),
            1,
            Blob::new(method.as_bytes()),
            Blob::new(b"{}"),
            42,
            &mut out,
        );
        (
            status,
            String::from_utf8_lossy(&buf[..out.len.min(buf.len())]).into_owned(),
        )
    }

    unsafe fn raw_isolate(handle: u64, service: &str) -> u64 {
        let iso = host_api().ctx_isolate.expect("ctx_isolate");
        let mut buf = vec![0u8; 256];
        let mut out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let status = iso(
            handle,
            Blob::new(service.as_bytes()),
            Blob::new(b""),
            &mut out,
        );
        assert_eq!(status, STATUS_OK);
        let text = String::from_utf8_lossy(&buf[..out.len]);
        let parsed: Value = serde_json::from_str(&text).expect("isolate json");
        parsed["handle"].as_u64().expect("derived handle")
    }

    PROVIDER_CALLS.store(0, Ordering::SeqCst);
    ALLOWED_HANDLE.store(0, Ordering::SeqCst);
    DENIED_HANDLE.store(0, Ordering::SeqCst);

    let app = App::new();
    let sink = Arc::new(RecordingTraceSink(Mutex::new(Vec::new())));
    assert!(app.set_trace_sink(sink.clone()), "install trace sink");

    set_method_allow_list_for(
        &app.root,
        Some(MethodAllowList::new().allow_plugin_method("t2-reader", "vault", "read")),
    );

    let provider = plugin_fn("t2-vault", Inject::default(), |ctx: Ctx, _| {
        apply_via_with_error_format(
            provider_entry,
            ctx,
            Config::Null,
            "t2-vault",
            ERROR_FORMAT_V1,
        )
    });
    let reader = plugin_fn("t2-reader", Inject::of(&["vault"]), |ctx: Ctx, _| {
        apply_via_with_error_format(
            caller_allowed_entry,
            ctx,
            Config::Null,
            "t2-reader",
            ERROR_FORMAT_V1,
        )
    });
    let intruder = plugin_fn("t2-intruder", Inject::of(&["vault"]), |ctx: Ctx, _| {
        apply_via_with_error_format(
            caller_denied_entry,
            ctx,
            Config::Null,
            "t2-intruder",
            ERROR_FORMAT_V1,
        )
    });

    let f_prov = app.root.plugin(provider, Config::Null);
    f_prov.settle().await;
    let f_read = app.root.plugin(reader, Config::Null);
    f_read.settle().await;
    let f_intr = app.root.plugin(intruder, Config::Null);
    f_intr.settle().await;

    let reader_h = ALLOWED_HANDLE.load(Ordering::SeqCst);
    let intruder_h = DENIED_HANDLE.load(Ordering::SeqCst);
    assert_ne!(reader_h, 0);
    assert_ne!(intruder_h, 0);

    // 1) t2-reader 调用被允许的 vault.read ⇒ 成功并真正执行提供者。
    let (st_ok, body_ok) = unsafe { raw_call_iface(reader_h, "vault", "read") };
    assert_eq!(st_ok, STATUS_OK, "allowed call failed: {body_ok}");
    let ok_json: Value = serde_json::from_str(&body_ok).unwrap();
    assert_eq!(ok_json["result"]["called"], "read");
    assert_eq!(PROVIDER_CALLS.load(Ordering::SeqCst), 1);

    // 2) t2-reader 调用未授权的 vault.secret ⇒ STATUS_ERR + 结构化 host_unauthorized / not_executed。
    let (st_deny, body_deny) = unsafe { raw_call_iface(reader_h, "vault", "secret") };
    assert_eq!(
        st_deny, STATUS_ERR,
        "unauthorized call must return STATUS_ERR: {body_deny}"
    );
    let payload = ErrorPayload::from_json(&body_deny).expect("structured ErrorPayload");
    assert_eq!(payload.code, "host_unauthorized");
    assert_eq!(payload.category, "authorization");
    assert_eq!(payload.certainty, "not_executed");
    assert_eq!(
        PROVIDER_CALLS.load(Ordering::SeqCst),
        1,
        "未授权调用不得执行提供者"
    );
    assert_eq!(dispatches_len(reader_h), 0, "未授权拒绝不得残留派发记录");

    // 3) 派生 isolate 句柄保留宿主绑定身份：不可绕过 vault.secret，仍可调用 vault.read（跨同域）。
    let derived_other = unsafe { raw_isolate(reader_h, "unrelated") };
    let (st_der_ok, body_der_ok) = unsafe { raw_call_iface(derived_other, "vault", "read") };
    assert_eq!(
        st_der_ok, STATUS_OK,
        "derived handle should retain read permission: {body_der_ok}"
    );
    assert_eq!(PROVIDER_CALLS.load(Ordering::SeqCst), 2);
    let (st_der_deny, body_der_deny) = unsafe { raw_call_iface(derived_other, "vault", "secret") };
    assert_eq!(st_der_deny, STATUS_ERR);
    assert_eq!(
        ErrorPayload::from_json(&body_der_deny).unwrap().code,
        "host_unauthorized"
    );
    assert_eq!(PROVIDER_CALLS.load(Ordering::SeqCst), 2);

    // 4) 未在白名单中的 t2-intruder 调用 vault.read ⇒ 拒绝。
    let (st_intr, body_intr) = unsafe { raw_call_iface(intruder_h, "vault", "read") };
    assert_eq!(st_intr, STATUS_ERR);
    assert_eq!(
        ErrorPayload::from_json(&body_intr).unwrap().code,
        "host_unauthorized"
    );
    assert_eq!(PROVIDER_CALLS.load(Ordering::SeqCst), 2);

    // 5) TraceSink 恰好记录了 3 条 host_unauthorized（call_id = 0）。
    {
        let records = sink.0.lock().unwrap();
        let denied_traces: Vec<&TraceRecord> = records
            .iter()
            .filter(|r| r.outcome == Outcome::Err("host_unauthorized".into()))
            .collect();
        assert_eq!(
            denied_traces.len(),
            3,
            "三次未授权 T2 调用应各自只产出一条 host_unauthorized trace 记录: {denied_traces:?}"
        );
        assert!(denied_traces.iter().all(|r| r.call_id == 0));
    }

    app.close().await;
}

// ============================================================================
// 深度审查回归用例
// ============================================================================

#[tokio::test]
async fn duplicate_id_cannot_poison_row_startup_or_be_swallowed_by_optional() {
    let dir = TempDir::new("dup-id-startup");

    // 1) 首行 optional: true + 第二行重复 id ⇒ strict=true 下仍必须 fail loud，不得被 optional 吞掉。
    let cfg_opt_first = dir.write(
        "opt-first.yml",
        "- id: dup\n  name: ok-a\n  optional: true\n- id: dup\n  name: ok-b\n",
    );
    let app1 = App::new();
    let err1 = load_with_source(
        app1.root.clone(),
        LoaderConfig::new(cfg_opt_first.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("ok-a", Ok(())),
            ("ok-b", Ok(())),
        ])),
    )
    .await
    .expect_err("首行 optional 不得吞掉重复 id 冲突");
    assert!(err1.to_string().contains("重复 label"), "{err1}");
    app1.close().await;

    // 2) 首行 required（缺省）+ 第二行重复 id 标 optional: true ⇒ 第二行不得污染首行启动策略。
    let cfg_req_first = dir.write(
        "req-first.yml",
        "- id: dup\n  name: broken-first\n- id: dup\n  name: ok-second\n  optional: true\n",
    );
    let app2 = App::new();
    set_startup_policy_for(
        &app2.root,
        Some(StartupPolicy::new().with_persisted_state(FailureDisposition::Degrade)),
    );
    let err2 = load_with_source(
        app2.root.clone(),
        LoaderConfig::new(cfg_req_first.to_string_lossy().into_owned()),
        Arc::new(ClassifiedTestSource::new([
            ("broken-first", Err(LoadFailure::runtime("first failed"))),
            ("ok-second", Ok(())),
        ])),
    )
    .await
    .expect_err("重复行标 optional 不得把首行污染为 optional");
    assert!(
        err2.to_string().contains("startup=required"),
        "首行必须保持 required: {err2}"
    );
    app2.close().await;

    // 3) 显式允许 PersistedState 降级时：首行正常挂载，且重复 label 失败台账不被首行 Mount 误清；
    //    修复重复 id 后再次对账，重复台账自动清空。
    let app3 = App::new();
    set_startup_policy_for(
        &app3.root,
        Some(StartupPolicy::new().with_persisted_state(FailureDisposition::Degrade)),
    );
    let source = Arc::new(ClassifiedTestSource::new([
        ("ok-a", Ok(())),
        ("ok-b", Ok(())),
    ]));
    let state = load_with_source(
        app3.root.clone(),
        LoaderConfig::new(cfg_opt_first.to_string_lossy().into_owned()),
        source,
    )
    .await
    .expect("PersistedState=Degrade 应降级重复 id 并挂载首行");
    assert_eq!(state.mounted_labels(), vec!["dup".to_string()]);
    let failed = state.failed_rows();
    assert_eq!(
        failed.len(),
        1,
        "首行 Mount 成功后不得抹掉重复 id 台账: {failed:?}"
    );
    assert_eq!(failed[0].label, "dup#duplicate");
    assert_eq!(failed[0].category, FailureCategory::PersistedState);
    assert!(failed[0].degraded);

    let fixed =
        parse_entries("- id: dup\n  name: ok-a\n  optional: true\n- id: dup2\n  name: ok-b\n")
            .expect("parse fixed");
    let outcome = state.reconcile_once(fixed).await;
    assert!(outcome.failures.is_empty(), "{outcome:?}");
    assert_eq!(
        state.mounted_labels(),
        vec!["dup".to_string(), "dup2".to_string()]
    );
    assert!(
        state.failed_rows().is_empty(),
        "消除重复 id 后失败台账必须清空: {:?}",
        state.failed_rows()
    );
    app3.close().await;
}

#[tokio::test]
async fn replace_revert_set_disabled_rollback_and_broken_patch_reload_keep_report_consistent() {
    let dir = TempDir::new("reconcile-consistency");
    let main_path = dir.write(
        "cordis.yml",
        "- id: svc\n  name: good\n- id: dormant-bad\n  name: bad\n  disabled: true\n",
    );
    let patch_path = dir.write("cordis.patch.yml", "[]\n");

    let source = Arc::new(ClassifiedTestSource::new([
        ("good", Ok(())),
        ("bad", Err(LoadFailure::runtime("bad plugin binary"))),
    ]));
    let app = App::new();
    let mut cfg = LoaderConfig::new(main_path.to_string_lossy().into_owned());
    cfg.patches = vec![patch_path.to_string_lossy().into_owned()];
    let state = load_with_source(app.root.clone(), cfg, source)
        .await
        .expect("initial load");
    assert_eq!(state.mounted_labels(), vec!["svc".to_string()]);

    // 1) Action::Replace 预检失败后保留旧实例并记录 failed_rows；配置回退到 good 后无 diff 收敛，
    //    过期 failed_rows 必须自动清除。
    let replace_to_bad =
        parse_entries("- id: svc\n  name: bad\n- id: dormant-bad\n  name: bad\n  disabled: true\n")
            .expect("parse replace");
    let outcome_fail = state.reconcile_once(replace_to_bad).await;
    assert_eq!(outcome_fail.failures.len(), 1);
    assert_eq!(state.failed_rows().len(), 1);
    assert_eq!(state.failed_rows()[0].label, "svc");

    let revert_to_good = parse_entries(
        "- id: svc\n  name: good\n- id: dormant-bad\n  name: bad\n  disabled: true\n",
    )
    .expect("parse revert");
    let outcome_revert = state.reconcile_once(revert_to_good).await;
    assert!(outcome_revert.is_noop(), "{outcome_revert:?}");
    assert!(
        state.failed_rows().is_empty(),
        "配置回退收敛后不得残留过期 failed_rows: {:?}",
        state.failed_rows()
    );
    assert!(state.report().get("failedRows").is_none());

    // 2) set_disabled 尝试启用坏行失败回滚后，disabled / failed_rows / loader.report 必须恢复一致。
    let toggle_err = state
        .set_disabled("dormant-bad", Some(false))
        .await
        .expect_err("启用坏行必须失败并回滚");
    assert!(toggle_err.contains("bad plugin binary"), "{toggle_err}");
    assert!(
        state.failed_rows().is_empty(),
        "回滚为 disabled 后不得残留 failed_rows: {:?}",
        state.failed_rows()
    );
    let report = state.report();
    assert_eq!(report["disabled"], json!(["dormant-bad"]));
    assert!(report.get("failedRows").is_none());

    // 3) reload 遇到损坏的 patch 文件时，persisted_state_errors 必须归属到精确的 patch 路径而非主文件；
    //    修复 patch 文件并重新 reload 后自动清空。
    dir.write("cordis.patch.yml", "not: [valid: yaml");
    let bad_reload = state.reload().await;
    assert!(
        matches!(bad_reload, ReloadOutcome::Failed(_)),
        "损坏 patch 的 reload 必须失败: {bad_reload:?}"
    );
    let persisted_errs = state.persisted_state_errors();
    assert_eq!(persisted_errs.len(), 1, "{persisted_errs:?}");
    assert_eq!(
        persisted_errs[0].label,
        patch_path.display().to_string(),
        "坏 patch 错误必须归属到 patch 路径而不是主配置"
    );

    dir.write("cordis.patch.yml", "[]\n");
    let ok_reload = state.reload().await;
    assert!(
        matches!(ok_reload, ReloadOutcome::Reconciled(_)),
        "修复 patch 后 reload 应成功: {ok_reload:?}"
    );
    assert!(
        state.persisted_state_errors().is_empty(),
        "reload 恢复后持久化状态错误应清空: {:?}",
        state.persisted_state_errors()
    );

    app.close().await;
}

#[tokio::test]
async fn explicit_dependency_plan_skips_plan_entry_and_anonymous_rows_correlate_from_preflight_cache(
) {
    struct CountingPlanSource {
        plan_calls: Arc<AtomicUsize>,
    }

    impl PluginSource for CountingPlanSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            match entry.name.as_str() {
                "db-anon" => Err("db binary down".into()),
                "consumer" => Ok(loaded_memory(
                    "consumer",
                    plugin_fn("consumer", Inject::of(&["db@1"]), |_ctx, _| {
                        Box::pin(async { Ok(()) })
                    }),
                )),
                other => Err(format!("unknown {other}")),
            }
        }

        fn plan_entry(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
        ) -> std::result::Result<Option<PlanEntrySpec>, String> {
            self.plan_calls.fetch_add(1, Ordering::SeqCst);
            if entry.name == "db-anon" {
                let mut provides = BTreeMap::new();
                provides.insert("db@1".into(), "1.0.0".into());
                return Ok(Some(PlanEntrySpec {
                    id: "db-anon".into(),
                    version: "1.0.0".into(),
                    provides,
                    requires: BTreeMap::new(),
                }));
            }
            Ok(None)
        }
    }

    // 1) 显式 LoaderConfig.dependency_plan 覆盖模式：绝不调用 source.plan_entry，
    //    同时仍可通过 dependency_plan 关联下游 Pending。
    let dir = TempDir::new("explicit-plan-and-anon");
    let cfg_explicit = dir.write(
        "explicit.yml",
        "- id: explicit-db\n  name: db-anon\n  optional: true\n- id: explicit-cons\n  name: consumer\n",
    );
    let explicit_calls = Arc::new(AtomicUsize::new(0));
    let mut provides = BTreeMap::new();
    provides.insert("db@1".into(), "1.0.0".into());
    let mut requires = BTreeMap::new();
    requires.insert("db@1".into(), "^1".into());
    let mut cfg = LoaderConfig::new(cfg_explicit.to_string_lossy().into_owned());
    cfg.dependency_plan = vec![
        PlanEntrySpec {
            id: "db-anon".into(),
            version: "1.0.0".into(),
            provides,
            requires: BTreeMap::new(),
        },
        PlanEntrySpec {
            id: "consumer".into(),
            version: "1.0.0".into(),
            provides: BTreeMap::new(),
            requires,
        },
    ];
    let app1 = App::new();
    let state1 = load_with_source(
        app1.root.clone(),
        cfg,
        Arc::new(CountingPlanSource {
            plan_calls: explicit_calls.clone(),
        }),
    )
    .await
    .expect("explicit plan load");
    assert_eq!(
        explicit_calls.load(Ordering::SeqCst),
        0,
        "显式 dependency_plan 覆盖模式绝不得调用 PluginSource::plan_entry"
    );
    let corr1 = state1.correlated_pending_report();
    assert_eq!(corr1.len(), 1);
    assert_eq!(corr1[0].missing[0].failed_upstream[0].label, "explicit-db");
    app1.close().await;

    // 2) 匿名行（无 id，且前面有静态 disabled 的同名行 ⇒ 实际行标为 db-anon#1）：
    //    预检缓存必须按稳定行标 db-anon#1 命中（不再二次调用 plan_entry），并正确关联下游 Pending。
    let cfg_anon = dir.write(
        "anon.yml",
        "- name: db-anon\n  disabled: true\n- name: db-anon\n  optional: true\n- name: consumer\n",
    );
    let anon_calls = Arc::new(AtomicUsize::new(0));
    let app2 = App::new();
    let state2 = load_with_source(
        app2.root.clone(),
        LoaderConfig::new(cfg_anon.to_string_lossy().into_owned()),
        Arc::new(CountingPlanSource {
            plan_calls: anon_calls.clone(),
        }),
    )
    .await
    .expect("anonymous rows load");
    // 预检阶段恰好查两次（db-anon#1 与 consumer#0），失败时命中 preflight_provides 缓存不再回查 plan_entry。
    assert_eq!(
        anon_calls.load(Ordering::SeqCst),
        2,
        "匿名行失败时应命中 preflight_provides 缓存而不重复查询 plan_entry"
    );
    let corr2 = state2.correlated_pending_report();
    assert_eq!(corr2.len(), 1);
    assert_eq!(corr2[0].label, "consumer#0");
    assert_eq!(corr2[0].missing[0].failed_upstream.len(), 1);
    assert_eq!(corr2[0].missing[0].failed_upstream[0].label, "db-anon#1");
    app2.close().await;
}

#[tokio::test]
async fn loader_mounted_t2_rows_enforce_per_row_allow_list_and_ctx_use_child_does_not_inherit() {
    static T2_VAULT_CALLS: AtomicUsize = AtomicUsize::new(0);
    static T2_RO_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    static T2_RW_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn t2_vault_entry(
        handle: u64,
        op: u32,
        input: Blob,
        _reserved: Blob,
        out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                let mut buf = vec![0u8; 512];
                let mut host_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let provide = host_api().ctx_provide_iface.expect("ctx_provide_iface");
                provide(
                    handle,
                    Blob::new(b"t2vault"),
                    1,
                    Blob::new(br#"["read","write"]"#),
                    1,
                    &mut host_out,
                )
            }
            OP_CALLBACK => {
                T2_VAULT_CALLS.fetch_add(1, Ordering::SeqCst);
                let req: Value = serde_json::from_slice(input.as_bytes()).unwrap_or(Value::Null);
                let reply = json!({ "ret": { "method": req["method"] } }).to_string();
                let bytes = reply.as_bytes();
                let out_ref = &mut *out;
                out_ref.len = bytes.len();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), out_ref.ptr, bytes.len());
                STATUS_OK
            }
            _ => STATUS_OK,
        }
    }

    unsafe extern "C" fn t2_worker_entry(
        handle: u64,
        op: u32,
        config: Blob,
        _reserved: Blob,
        _out: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            let parsed: Value = serde_json::from_slice(config.as_bytes()).unwrap_or(Value::Null);
            match parsed.get("slot").and_then(Value::as_str) {
                Some("ro") => T2_RO_HANDLE.store(handle, Ordering::SeqCst),
                Some("rw") => T2_RW_HANDLE.store(handle, Ordering::SeqCst),
                _ => {}
            }
        }
        STATUS_OK
    }

    struct ChildProbeRegistry;
    impl NativeRegistry for ChildProbeRegistry {
        fn native(&self, name: &str) -> Option<Arc<dyn Plugin>> {
            if name == "t2-child-probe" {
                Some(plugin_fn("t2-child-probe", Inject::default(), |_ctx, _| {
                    Box::pin(async { Ok(()) })
                }))
            } else {
                None
            }
        }
    }

    struct T2LoaderSource;
    impl PluginSource for T2LoaderSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
                "t2-vault" => with_provision(
                    plugin_fn("t2-vault", Inject::default(), |ctx: Ctx, cfg: Config| {
                        apply_via_with_error_format(
                            t2_vault_entry,
                            ctx,
                            cfg,
                            "t2-vault",
                            ERROR_FORMAT_V1,
                        )
                    }),
                    vec!["t2vault".into()],
                ),
                "t2-worker" => plugin_fn(
                    "t2-worker",
                    Inject::of(&["t2vault"]),
                    |ctx: Ctx, cfg: Config| {
                        apply_via_with_error_format(
                            t2_worker_entry,
                            ctx,
                            cfg,
                            "t2-worker",
                            ERROR_FORMAT_V1,
                        )
                    },
                ),
                other => return Err(format!("unknown {other}")),
            };
            Ok(loaded_memory(&entry.name, plugin))
        }
    }

    unsafe fn raw_call(handle: u64, method: &str) -> (i32, String) {
        let call = host_api().ctx_call_iface.expect("ctx_call_iface");
        let mut buf = vec![0u8; 4096];
        let mut out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let status = call(
            handle,
            Blob::new(b"t2vault"),
            1,
            Blob::new(method.as_bytes()),
            Blob::new(b"{}"),
            99,
            &mut out,
        );
        (
            status,
            String::from_utf8_lossy(&buf[..out.len.min(buf.len())]).into_owned(),
        )
    }

    unsafe fn raw_use_native(handle: u64, name: &str) -> u64 {
        let use_fn = host_api().ctx_use.expect("ctx_use");
        let mut buf = vec![0u8; 512];
        let mut out = Out {
            ptr: buf.as_mut_ptr(),
            cap: buf.len(),
            len: 0,
        };
        let url = format!("@native:{name}");
        let status = use_fn(
            handle,
            Blob::new(url.as_bytes()),
            Blob::new(b"{}"),
            &mut out,
        );
        let text = String::from_utf8_lossy(&buf[..out.len.min(buf.len())]).into_owned();
        assert_eq!(status, STATUS_OK, "ctx_use failed: {text}");
        let parsed: Value = serde_json::from_str(&text).expect("ctx_use json");
        parsed["handle"].as_u64().expect("child handle")
    }

    T2_VAULT_CALLS.store(0, Ordering::SeqCst);
    T2_RO_HANDLE.store(0, Ordering::SeqCst);
    T2_RW_HANDLE.store(0, Ordering::SeqCst);
    set_native_registry(Arc::new(ChildProbeRegistry));

    let dir = TempDir::new("t2-row-allow-list");
    let cfg_path = dir.write(
        "cordis.yml",
        "\
- id: vault-row
  name: t2-vault
- id: t2-ro
  name: t2-worker
  config:
    slot: ro
- id: t2-rw
  name: t2-worker
  config:
    slot: rw
",
    );

    let app = App::new();
    assert!(!app.store.has_trace_sink(), "必须不依赖 TraceSink");
    set_method_allow_list_for(
        &app.root,
        Some(
            MethodAllowList::new()
                .allow_row_method("t2-ro", "t2vault", "read")
                .allow_row_methods("t2-rw", "t2vault", ["read", "write"]),
        ),
    );

    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(T2LoaderSource),
    )
    .await
    .expect("load t2 rows");
    state.fiber_of("t2-ro").unwrap().settle().await;
    state.fiber_of("t2-rw").unwrap().settle().await;

    let ro_h = T2_RO_HANDLE.load(Ordering::SeqCst);
    let rw_h = T2_RW_HANDLE.load(Ordering::SeqCst);
    assert_ne!(ro_h, 0);
    assert_ne!(rw_h, 0);

    // 1) 同插件不同行按宿主绑定的 row_label 区分授权。
    let (st, body) = unsafe { raw_call(ro_h, "read") };
    assert_eq!(st, STATUS_OK, "t2-ro read should pass: {body}");
    assert_eq!(T2_VAULT_CALLS.load(Ordering::SeqCst), 1);

    let (st, body) = unsafe { raw_call(ro_h, "write") };
    assert_eq!(st, STATUS_ERR, "t2-ro write must be denied: {body}");
    let payload = ErrorPayload::from_json(&body).expect("structured payload");
    assert_eq!(payload.code, "host_unauthorized");
    assert_eq!(payload.category, "authorization");
    assert_eq!(payload.certainty, "not_executed");
    assert_eq!(T2_VAULT_CALLS.load(Ordering::SeqCst), 1);

    let (st, body) = unsafe { raw_call(rw_h, "write") };
    assert_eq!(st, STATUS_OK, "t2-rw write should pass: {body}");
    assert_eq!(T2_VAULT_CALLS.load(Ordering::SeqCst), 2);

    // 2) T2 经 ctx_use 挂载的子组件拥有独立 child fiber（host_label = None），
    //    绝不继承父行 t2-rw 的 row_label 权限！
    let child_h = unsafe { raw_use_native(rw_h, "t2-child-probe") };
    let (st, body) = unsafe { raw_call(child_h, "write") };
    assert_eq!(
        st, STATUS_ERR,
        "ctx_use 子组件派生句柄不得继承父行 t2-rw 的授权: {body}"
    );
    assert_eq!(
        ErrorPayload::from_json(&body).unwrap().code,
        "host_unauthorized"
    );
    assert_eq!(
        T2_VAULT_CALLS.load(Ordering::SeqCst),
        2,
        "被拒子组件调用不得进入提供者"
    );

    // 父行 t2-rw 自身权限不受影响。
    let (st, body) = unsafe { raw_call(rw_h, "write") };
    assert_eq!(st, STATUS_OK, "t2-rw write should still pass: {body}");
    assert_eq!(T2_VAULT_CALLS.load(Ordering::SeqCst), 3);

    app.close().await;
}

// ============================================================================
// 二轮评审加固回归用例（P1-1 / P1-3 / P2-5 / P2-6）
// ============================================================================

#[tokio::test]
async fn pending_report_correlates_upstream_only_within_matching_realm() {
    struct RealmSource;
    impl PluginSource for RealmSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
                "db-fail" => with_provision(
                    plugin_fn("db-fail", Inject::default(), |_ctx, _| {
                        Box::pin(async { Err("db init failed".into()) })
                    }),
                    vec!["db".into()],
                ),
                "db-consumer" => plugin_fn("db-consumer", Inject::of(&["db"]), |_ctx, _| {
                    Box::pin(async { Ok(()) })
                }),
                other => return Err(format!("unknown {other}")),
            };
            Ok(loaded_memory(&entry.name, plugin))
        }
    }

    let dir = TempDir::new("pending-realm-filter");
    let cfg_path = dir.write(
        "cordis.yml",
        "\
- id: db-priv
  name: db-fail
  optional: true
  isolate:
    db: true
- id: db-tenant-a
  name: db-fail
  optional: true
  isolate:
    db: tenant-a
- id: cons-default
  name: db-consumer
- id: cons-tenant-a
  name: db-consumer
  isolate:
    db: tenant-a
- id: cons-tenant-b
  name: db-consumer
  isolate:
    db: tenant-b
",
    );

    let app = App::new();
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(RealmSource),
    )
    .await
    .expect("可选上游失败应降级启动");

    let default_realms_before = app.store.default_realm_count();
    let report = state.report();
    assert_eq!(
        app.store.default_realm_count(),
        default_realms_before,
        "correlated_pending_report 必须保持零域分配只读不变量"
    );

    let pending = report["pending"].as_array().expect("pending array");
    let find = |label: &str| {
        pending
            .iter()
            .find(|row| row["label"] == label)
            .expect("pending row")
    };

    // 默认域与 tenant-b 不可见 db-priv / db-tenant-a，绝不得跨隔离域误关联。
    assert!(
        find("cons-default")["missing"][0]
            .get("failedUpstream")
            .is_none(),
        "默认域不得误关联私有域或 tenant-a 的失败上游: {}",
        find("cons-default")
    );
    assert!(
        find("cons-tenant-b")["missing"][0]
            .get("failedUpstream")
            .is_none(),
        "tenant-b 不得误关联 tenant-a 或私有域的失败上游: {}",
        find("cons-tenant-b")
    );

    // tenant-a 只应精确关联同属 tenant-a 的 db-tenant-a，绝不混入 db-priv。
    let tenant_a_up = find("cons-tenant-a")["missing"][0]["failedUpstream"]
        .as_array()
        .expect("tenant-a 必须关联到同域失败上游");
    assert_eq!(tenant_a_up.len(), 1, "{tenant_a_up:?}");
    assert_eq!(tenant_a_up[0]["label"], "db-tenant-a");

    app.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconcile_dispose_and_replace_respect_dispose_budget_and_block_remount_while_draining() {
    struct StuckReconcileSource {
        release: Arc<tokio::sync::Notify>,
        v2_applies: Arc<AtomicUsize>,
    }

    impl PluginSource for StuckReconcileSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            let plugin: Arc<dyn Plugin> = match entry.name.as_str() {
                "stuck-v1" => {
                    let release = self.release.clone();
                    plugin_fn("stuck-v1", Inject::default(), move |ctx: Ctx, _| {
                        let release = release.clone();
                        Box::pin(async move {
                            ctx.effect(move |_| {
                                Disposable::new(move || {
                                    Box::pin(async move {
                                        release.notified().await;
                                    })
                                })
                            });
                            Ok(())
                        })
                    })
                }
                "clean-v2" => {
                    let applies = self.v2_applies.clone();
                    plugin_fn("clean-v2", Inject::default(), move |_ctx: Ctx, _| {
                        applies.fetch_add(1, Ordering::SeqCst);
                        Box::pin(async { Ok(()) })
                    })
                }
                other => return Err(format!("unknown {other}")),
            };
            Ok(loaded_memory(&entry.name, plugin))
        }
    }

    let dir = TempDir::new("draining-reconcile");
    let cfg_path = dir.write("cordis.yml", "- id: svc\n  name: stuck-v1\n");

    let app = App::new();
    set_lifecycle_policy_for(
        &app.root,
        LifecyclePolicy::new(None, None, Some(50)).unwrap(),
    )
    .unwrap();

    let release_a = Arc::new(tokio::sync::Notify::new());
    let v2_applies = Arc::new(AtomicUsize::new(0));
    let state = load_with_source(
        app.root.clone(),
        LoaderConfig::new(cfg_path.to_string_lossy().into_owned()),
        Arc::new(StuckReconcileSource {
            release: release_a.clone(),
            v2_applies: v2_applies.clone(),
        }),
    )
    .await
    .expect("initial mount");
    assert_eq!(state.mounted_labels(), vec!["svc".to_string()]);

    // 1) Action::Replace 卸载卡住的旧实例：必须受 dispose_timeout_ms 约束及时返回，
    //    且在后台清理未完成时拒绝挂载 clean-v2。
    let replace_entries = parse_entries("- id: svc\n  name: clean-v2\n").unwrap();
    let replace_outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.reconcile_once(replace_entries),
    )
    .await
    .expect("Action::Replace 必须受 dispose_timeout_ms 约束不得无限挂死");
    assert!(!replace_outcome.failures.is_empty(), "{replace_outcome:?}");
    assert!(
        state.mounted_labels().is_empty(),
        "超时后旧实例已摘表且新实例不得挂载"
    );
    assert_eq!(v2_applies.load(Ordering::SeqCst), 0);

    // 2) 后台清理未完时再次 sync()（Action::Mount）：必须继续被 draining 阻断，不得旁路重挂。
    let blocked_mount = state.sync().await;
    assert!(
        blocked_mount
            .failures
            .iter()
            .any(|msg| msg.contains("后台清理尚未完成")),
        "后台清理未完时必须阻断重挂: {blocked_mount:?}"
    );
    assert_eq!(v2_applies.load(Ordering::SeqCst), 0);
    assert!(state.mounted_labels().is_empty());

    // 3) 放行旧实例 disposer 并等待后台清理归零后，再次 sync() 自动恢复挂载 clean-v2。
    release_a.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.store.wait_background_cleanups(),
    )
    .await
    .expect("后台清理收敛");
    let recovered = state.sync().await;
    assert!(recovered.failures.is_empty(), "{recovered:?}");
    assert_eq!(recovered.mounted, vec!["svc".to_string()]);
    assert_eq!(v2_applies.load(Ordering::SeqCst), 1);
    assert!(state.failed_rows().is_empty());

    // 4) HMR reload_one_with_plugin 触发旧实例超时（Lost）后，同样阻断后续 sync() 旁路重挂，
    //    待清理结束后恢复。
    let release_b = Arc::new(tokio::sync::Notify::new());
    let release_b_d = release_b.clone();
    let stuck_for_reload = plugin_fn("stuck-hmr", Inject::default(), move |ctx: Ctx, _| {
        let rel = release_b_d.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::new(move || {
                    Box::pin(async move {
                        rel.notified().await;
                    })
                })
            });
            Ok(())
        })
    });
    let r_install = reload_one_with_plugin(&app.root, &state, "svc", stuck_for_reload).await;
    assert_eq!(r_install.status, ReloadStatus::Active);

    let dummy_next = plugin_fn("dummy-next", Inject::default(), |_ctx, _| {
        Box::pin(async { Ok(()) })
    });
    let r_lost = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reload_one_with_plugin(&app.root, &state, "svc", dummy_next),
    )
    .await
    .expect("reload_one_with_plugin 应按 dispose_timeout_ms 及时返回");
    assert_eq!(r_lost.status, ReloadStatus::Lost);

    let blocked_after_lost = state.sync().await;
    assert!(
        blocked_after_lost
            .failures
            .iter()
            .any(|msg| msg.contains("后台清理尚未完成")),
        "ReloadStatus::Lost 后后台清理未完时 sync() 不得旁路重挂: {blocked_after_lost:?}"
    );
    assert_eq!(v2_applies.load(Ordering::SeqCst), 1);

    release_b.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.store.wait_background_cleanups(),
    )
    .await
    .expect("HMR 后台清理收敛");
    let recovered2 = state.sync().await;
    assert!(recovered2.failures.is_empty(), "{recovered2:?}");
    assert_eq!(v2_applies.load(Ordering::SeqCst), 2);

    // 5) set_disabled("svc", Some(true)) 触发 Action::Dispose 超时：及时返回 Err，
    //    且因为旧实例已实际摘表处置，不把树错误回滚回 disabled=false。
    let release_c = Arc::new(tokio::sync::Notify::new());
    let release_c_d = release_c.clone();
    let stuck_for_disable = plugin_fn("stuck-disable", Inject::default(), move |ctx: Ctx, _| {
        let rel = release_c_d.clone();
        Box::pin(async move {
            ctx.effect(move |_| {
                Disposable::new(move || {
                    Box::pin(async move {
                        rel.notified().await;
                    })
                })
            });
            Ok(())
        })
    });
    assert_eq!(
        reload_one_with_plugin(&app.root, &state, "svc", stuck_for_disable)
            .await
            .status,
        ReloadStatus::Active
    );
    let disable_err = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        state.set_disabled("svc", Some(true)),
    )
    .await
    .expect("set_disabled 禁用卡死行必须受 dispose_timeout_ms 约束")
    .expect_err("卸载超时应向调用方报出错误");
    assert!(disable_err.contains("后台清理仍在进行"), "{disable_err}");
    assert!(
        state.tree_snapshot()[0].disabled,
        "已摘表处置的行不得被回滚回 disabled=false"
    );
    assert!(state.mounted_labels().is_empty());

    release_c.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        app.store.wait_background_cleanups(),
    )
    .await
    .expect("禁用后台清理收敛");
    app.close().await;
}

#[tokio::test]
async fn reload_patch_io_error_attributes_exact_path_and_t2_auth_race_stays_status_err() {
    // 1) P2-6：运行期删除 patch 文件触发 I/O 失败，persisted_state_errors 必须归属到精确的 patch 路径而非主配置。
    let dir = TempDir::new("patch-io-attribution");
    let main_path = dir.write("cordis.yml", "- id: core\n  name: core\n");
    let patch_path = dir.write("cordis.patch.yml", "[]\n");

    let app = App::new();
    let mut cfg = LoaderConfig::new(main_path.to_string_lossy().into_owned());
    cfg.patches = vec![patch_path.to_string_lossy().into_owned()];
    let state = load_with_source(
        app.root.clone(),
        cfg,
        Arc::new(ClassifiedTestSource::new([("core", Ok(()))])),
    )
    .await
    .expect("initial load");

    std::fs::remove_file(&patch_path).expect("remove patch file");
    let missing_reload = state.reload().await;
    match &missing_reload {
        ReloadOutcome::Failed(msg) => assert!(
            msg.contains("cordis.patch.yml"),
            "I/O 失败消息必须指向缺失的 patch 文件: {msg}"
        ),
        other => panic!("expected Failed on missing patch, got {other:?}"),
    }
    let errs = state.persisted_state_errors();
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert_eq!(
        errs[0].label,
        patch_path.display().to_string(),
        "persisted_state_errors 必须归属到精确的 patch 路径而非 cordis.yml"
    );

    dir.write("cordis.patch.yml", "[]\n");
    assert!(matches!(state.reload().await, ReloadOutcome::Reconciled(_)));
    assert!(state.persisted_state_errors().is_empty());
    app.close().await;

    // 2) P2-5：确定性复现并发收紧白名单（第一次前置检查放行、第二次 call_iface 复检拒绝）——
    //    T2 ctx_call_iface 必须依然返回 STATUS_ERR + ErrorPayload(host_unauthorized)，不得漂移为 STATUS_OK 内联错。
    static RACE_PROVIDER_CALLS: AtomicUsize = AtomicUsize::new(0);
    static RACE_CALLER_HANDLE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn race_provider(
        handle: u64,
        op: u32,
        _input: Blob,
        _reserved: Blob,
        _out: *mut Out,
    ) -> i32 {
        match op {
            OP_APPLY => {
                let mut buf = vec![0u8; 512];
                let mut host_out = Out {
                    ptr: buf.as_mut_ptr(),
                    cap: buf.len(),
                    len: 0,
                };
                let provide = host_api().ctx_provide_iface.expect("ctx_provide_iface");
                provide(
                    handle,
                    Blob::new(b"race_svc"),
                    1,
                    Blob::new(br#"["ping"]"#),
                    1,
                    &mut host_out,
                )
            }
            OP_CALLBACK => {
                RACE_PROVIDER_CALLS.fetch_add(1, Ordering::SeqCst);
                STATUS_OK
            }
            _ => STATUS_OK,
        }
    }

    unsafe extern "C" fn race_caller(
        handle: u64,
        op: u32,
        _a: Blob,
        _b: Blob,
        _out: *mut Out,
    ) -> i32 {
        if op == OP_APPLY {
            RACE_CALLER_HANDLE.store(handle, Ordering::SeqCst);
        }
        STATUS_OK
    }

    RACE_PROVIDER_CALLS.store(0, Ordering::SeqCst);
    RACE_CALLER_HANDLE.store(0, Ordering::SeqCst);

    let app2 = App::new();
    let f_prov = app2.root.plugin(
        plugin_fn("race-prov", Inject::default(), |ctx: Ctx, _| {
            apply_via_with_error_format(
                race_provider,
                ctx,
                Config::Null,
                "race-prov",
                ERROR_FORMAT_V1,
            )
        }),
        Config::Null,
    );
    f_prov.settle().await;
    let f_caller = app2.root.plugin(
        plugin_fn("race-caller", Inject::of(&["race_svc"]), |ctx: Ctx, _| {
            apply_via_with_error_format(
                race_caller,
                ctx,
                Config::Null,
                "race-caller",
                ERROR_FORMAT_V1,
            )
        }),
        Config::Null,
    );
    f_caller.settle().await;

    // 模拟宿主在两次授权检查之间收紧白名单：第 1 次检查放行，第 2 次拒绝。
    let checks = Arc::new(AtomicUsize::new(0));
    let checks_c = checks.clone();
    app2.store
        .set_call_auth_hook(Some(Arc::new(move |_caller, _service, _method| {
            if checks_c.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("concurrently tightened policy".to_string())
            }
        })));

    let handle = RACE_CALLER_HANDLE.load(Ordering::SeqCst);
    assert_ne!(handle, 0);
    let call = host_api().ctx_call_iface.expect("ctx_call_iface");
    let mut buf = vec![0u8; 4096];
    let mut out = Out {
        ptr: buf.as_mut_ptr(),
        cap: buf.len(),
        len: 0,
    };
    let status = unsafe {
        call(
            handle,
            Blob::new(b"race_svc"),
            1,
            Blob::new(b"ping"),
            Blob::new(b"{}"),
            7,
            &mut out,
        )
    };
    let body = String::from_utf8_lossy(&buf[..out.len.min(buf.len())]).into_owned();
    assert_eq!(
        status, STATUS_ERR,
        "并发收紧白名单时也必须统一返回 STATUS_ERR 而非 STATUS_OK 内联错: {body}"
    );
    let payload = ErrorPayload::from_json(&body).expect("structured ErrorPayload");
    assert_eq!(payload.code, "host_unauthorized");
    assert_eq!(payload.category, "authorization");
    assert_eq!(payload.certainty, "not_executed");
    assert_eq!(RACE_PROVIDER_CALLS.load(Ordering::SeqCst), 0);
    assert_eq!(dispatches_len(handle), 0);

    app2.close().await;
}

// ============================================================================
// P1-2：strict_sidecars = true 依赖预检与行级 Optional / StartupPolicy 一致性
// ============================================================================

#[tokio::test]
async fn strict_sidecars_preflight_honors_optional_runtime_and_independent_startup_policy() {
    // 1) 真实 DiskPluginSource + PolicyPluginSource + strict_sidecars = true：
    //    sidecar 的 Source 与 Integrity 错误同样按 StartupPolicy 独立裁决，且默认 fail-closed。
    let dir = TempDir::new("strict-sidecars-policy");
    let dll_name = artifact_filename("opt_sidecar");
    let dll_path = dir.write(&dll_name, "fake-artifact-bytes-v1");
    let actual_sha = cordis_host::source::sha256_file(&dll_path).expect("sha256");
    let manifest_path = cordis_host::source::manifest_path(&dll_path);
    let manifest_json = json!({
        "id": "opt_sidecar",
        "version": "1.0.0",
        "source_revision": "rev-1",
        "abi_version": cordis_abi::ABI_VERSION,
        "sha256": actual_sha,
        "provides": { "opt.svc": "1.0.0" },
        "requires": {}
    })
    .to_string();
    std::fs::write(&manifest_path, &manifest_json).expect("write manifest");

    let cfg_path = dir.write(
        "cordis.yml",
        "- id: opt\n  name: opt_sidecar\n  optional: true\n",
    );

    // 1a) artifact 不在 allow-list（Source 错误）+ strict_sidecars = true + 无 StartupPolicy ⇒ 必须 fail-closed。
    let empty_allow_source = Arc::new(PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        PluginSourcePolicy::new(vec![]),
    ));
    let mut cfg = LoaderConfig::new(cfg_path.to_string_lossy().into_owned());
    cfg.strict_sidecars = true;

    let app1 = App::new();
    let err1 = load_with_source(app1.root.clone(), cfg.clone(), empty_allow_source.clone())
        .await
        .expect_err("无 StartupPolicy 时 strict_sidecars 必须 fail-closed 拦截 Source 错误");
    assert!(err1.to_string().contains("allow-list"), "{err1}");
    app1.close().await;

    // 1b) 安装 StartupPolicy::with_source(Degrade)：strict_sidecars = true 应放行 Source 降级并记入 report。
    let app2 = App::new();
    set_startup_policy_for(
        &app2.root,
        Some(StartupPolicy::new().with_source(FailureDisposition::Degrade)),
    );
    let state2 = load_with_source(app2.root.clone(), cfg.clone(), empty_allow_source.clone())
        .await
        .expect("Source=Degrade 时 strict_sidecars 预检与装载均应按策略降级");
    assert!(state2.mounted_labels().is_empty());
    assert_eq!(state2.degraded_rows().len(), 1);
    assert_eq!(state2.degraded_rows()[0].category, FailureCategory::Source);
    assert_eq!(state2.report()["preflight"]["verdict"], "downgraded");
    app2.close().await;

    // 1c) 篡改 artifact 制造 Integrity（SHA256 不匹配）错误：仅 Source=Degrade 仍必须 fail-closed！
    std::fs::write(&dll_path, "tampered-artifact-bytes").expect("tamper artifact");
    let allow_original_source = Arc::new(PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        PluginSourcePolicy::new(vec![AllowedPlugin {
            id: "opt_sidecar".into(),
            version: "1.0.0".into(),
            source_revision: "rev-1".into(),
            abi_version: cordis_abi::ABI_VERSION,
            sha256: actual_sha,
        }]),
    ));
    let app3 = App::new();
    set_startup_policy_for(
        &app3.root,
        Some(StartupPolicy::new().with_source(FailureDisposition::Degrade)),
    );
    let err3 = load_with_source(
        app3.root.clone(),
        cfg.clone(),
        allow_original_source.clone(),
    )
    .await
    .expect_err("Source=Degrade 绝不得放行 strict_sidecars 下的 Integrity 篡改错误");
    assert!(err3.to_string().contains("SHA256"), "{err3}");
    app3.close().await;

    // 1d) 显式开启 Integrity=Degrade 后，strict_sidecars 预检与装载按 Integrity 策略降级。
    let app4 = App::new();
    set_startup_policy_for(
        &app4.root,
        Some(StartupPolicy::new().with_integrity(FailureDisposition::Degrade)),
    );
    let state4 = load_with_source(app4.root.clone(), cfg.clone(), allow_original_source)
        .await
        .expect("Integrity=Degrade 时 strict_sidecars 应按策略降级");
    assert_eq!(state4.degraded_rows().len(), 1);
    assert_eq!(
        state4.degraded_rows()[0].category,
        FailureCategory::Integrity
    );
    app4.close().await;

    // 2) 自定义来源 plan_entry 报错（默认归类 RuntimeUnavailable）+ strict_sidecars = true：
    //    required 行必须阻断启动；optional 行降级通过并让健康行正常挂载。
    struct FlakyPlanSource;
    impl PluginSource for FlakyPlanSource {
        fn load(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
            _revision: u64,
        ) -> std::result::Result<LoadedFile, String> {
            match entry.name.as_str() {
                "core" => Ok(loaded_memory(
                    "core",
                    plugin_fn("core", Inject::default(), |_ctx, _| {
                        Box::pin(async { Ok(()) })
                    }),
                )),
                "flaky" => Err("transient registry unavailable".into()),
                other => Err(format!("unknown {other}")),
            }
        }

        fn plan_entry(
            &self,
            entry: &Entry,
            _search: &[PathBuf],
        ) -> std::result::Result<Option<PlanEntrySpec>, String> {
            if entry.name == "flaky" {
                return Err("transient sidecar read error".into());
            }
            Ok(Some(PlanEntrySpec {
                id: entry.name.clone(),
                version: "1.0.0".into(),
                provides: BTreeMap::new(),
                requires: BTreeMap::new(),
            }))
        }
    }

    let req_cfg_path = dir.write(
        "req.yml",
        "- id: core\n  name: core\n- id: f\n  name: flaky\n",
    );
    let mut req_cfg = LoaderConfig::new(req_cfg_path.to_string_lossy().into_owned());
    req_cfg.strict_sidecars = true;
    let app5 = App::new();
    let err5 = load_with_source(app5.root.clone(), req_cfg, Arc::new(FlakyPlanSource))
        .await
        .expect_err("required 行在 strict_sidecars=true 下必须阻断启动");
    assert!(err5.to_string().contains("transient sidecar"), "{err5}");
    app5.close().await;

    let opt_cfg_path = dir.write(
        "opt.yml",
        "- id: core\n  name: core\n- id: f\n  name: flaky\n  optional: true\n",
    );
    let mut opt_cfg = LoaderConfig::new(opt_cfg_path.to_string_lossy().into_owned());
    opt_cfg.strict_sidecars = true;
    let app6 = App::new();
    let state6 = load_with_source(app6.root.clone(), opt_cfg, Arc::new(FlakyPlanSource))
        .await
        .expect("optional 行在 strict_sidecars=true 遇到 RuntimeUnavailable 时应降级通过");
    assert_eq!(state6.mounted_labels(), vec!["core".to_string()]);
    assert_eq!(state6.degraded_rows().len(), 1);
    assert_eq!(state6.degraded_rows()[0].label, "f");
    assert_eq!(state6.report()["preflight"]["verdict"], "downgraded");
    app6.close().await;
}
