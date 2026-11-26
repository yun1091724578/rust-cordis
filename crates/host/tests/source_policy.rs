use cordis_core::App;
use cordis_host::loader::{
    load_with_source, DiskPluginSource, InterpolationMode, LoaderConfig, PluginSource,
    PolicyPluginSource,
};
use cordis_host::patch::Entry;
use cordis_host::source::{
    active_policy, allow_unverified, set_active_policy, set_allow_unverified, AllowedPlugin,
    PluginSourcePolicy, SourceAdmission,
};
use cordis_host::{probe_capabilities_in, LoadedPlugin};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex, MutexGuard};

const SHA_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const SHA_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn fixture(name: &str, bytes: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cordis-source-policy-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plugin.dll");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn write_manifest(path: &Path, hash: &str, revision: &str) {
    let manifest = path.with_extension("dll.manifest.json");
    std::fs::write(
        manifest,
        serde_json::json!({
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": revision,
            "abi_version": 1,
            "sha256": hash,
        })
        .to_string(),
    )
    .unwrap();
}

fn allowed(hash: &str) -> AllowedPlugin {
    AllowedPlugin {
        id: "example.plugin".to_string(),
        version: "0.1.0".to_string(),
        source_revision: "abc123".to_string(),
        abi_version: 1,
        sha256: hash.to_string(),
    }
}

#[test]
fn verifies_a_manifest_matching_the_artifact_and_allow_list() {
    let path = fixture("ok", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);

    let verified = policy.verify(&path).expect("verified source");
    assert_eq!(verified.manifest.id, "example.plugin");
    assert_eq!(verified.manifest.source_revision, "abc123");
    assert_eq!(verified.sha256, SHA_ABC);
}

#[test]
fn rejects_missing_manifest() {
    let path = fixture("missing-manifest", b"abc");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("manifest"), "{error}");
}

#[test]
fn rejects_a_tampered_artifact() {
    let path = fixture("tampered", b"abc");
    write_manifest(&path, SHA_EMPTY, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("SHA256"), "{error}");
}

#[test]
fn rejects_an_artifact_that_is_not_in_the_allow_list() {
    let path = fixture("not-allowed", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("allow-list"), "{error}");
}

#[test]
fn rejects_a_manifest_without_source_revision() {
    let path = fixture("no-revision", b"abc");
    let manifest = path.with_extension("dll.manifest.json");
    std::fs::write(
        manifest,
        serde_json::json!({
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": "",
            "abi_version": 1,
            "sha256": SHA_ABC,
        })
        .to_string(),
    )
    .unwrap();
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("source_revision"), "{error}");
}

#[test]
fn open_verified_rejects_a_tampered_artifact_before_dlopen() {
    let path = fixture("open-verified", b"not a dynamic library");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_verified(&path, &policy) {
        Ok(_) => panic!("tampered artifact must not load"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("SHA256"), "{text}");
    assert!(
        !text.contains("LoadLibrary") && !text.contains("dlopen"),
        "{text}"
    );
}

#[test]
fn r2_p2_open_verified_stages_isolated_copy_before_opening() {
    let path = fixture("open-verified-isolated", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_verified(&path, &policy) {
        Ok(_) => panic!("non-DLL bytes must not load"),
        Err(error) => error.to_string(),
    };
    let source_str = path.to_string_lossy().into_owned();
    assert!(
        !error.contains(&source_str),
        "open_verified 必须从隔离暂存副本打开，而不是直接打开源路径 {source_str}：{error}"
    );
    assert_no_staged_copy(&path);
}

#[test]
fn open_staged_verified_refuses_to_delete_the_original_artifact() {
    let path = fixture("same-path", b"not a dynamic library");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_staged_verified(&path, &path, &policy) {
        Ok(_) => panic!("original path must not be adopted as a staged copy"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("暂存副本"), "{text}");
}

#[test]
fn open_staged_verified_rejects_different_staged_bytes() {
    let original = fixture("staged-original", b"abc");
    write_manifest(&original, SHA_ABC, "abc123");
    let staged = fixture("staged-copy", b"different");
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = match LoadedPlugin::open_staged_verified(&original, &staged, &policy) {
        Ok(_) => panic!("a staged copy with different bytes must not load"),
        Err(error) => error,
    };
    let text = error.to_string();
    assert!(text.contains("暂存副本 SHA256"), "{text}");
}

/// P1-1 的可选依赖字段：sidecar 里的 `provides` / `requires` 被读入 manifest，
/// allow-list 里的同名字段被忽略——两者都不能破坏既有准入校验。
#[test]
fn tolerates_optional_dependency_declarations_in_manifest_and_allow_list() {
    let path = fixture("dependency-fields", b"abc");
    let manifest = path.with_extension("dll.manifest.json");
    std::fs::write(
        &manifest,
        serde_json::json!({
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": "abc123",
            "abi_version": 1,
            "sha256": SHA_ABC,
            "provides": { "svc": "1.0.0" },
            "requires": { "other": "^1" },
        })
        .to_string(),
    )
    .unwrap();
    let allow_list = serde_json::json!({
        "allowed": [{
            "id": "example.plugin",
            "version": "0.1.0",
            "source_revision": "abc123",
            "abi_version": 1,
            "sha256": SHA_ABC,
            "provides": { "ignored": "9.9.9" },
        }],
    })
    .to_string();
    let policy = PluginSourcePolicy::from_json(&allow_list).expect("allow-list 必须可解析");

    let verified = policy.verify(&path).expect("可选依赖字段不得破坏准入");
    assert_eq!(verified.manifest.provides["svc"], "1.0.0");
    assert_eq!(verified.manifest.requires["other"], "^1");
}

#[test]
fn rejects_a_non_semver_manifest_version() {
    let path = fixture("non-semver", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let manifest_path = cordis_host::source::manifest_path(&path);
    let text = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(&manifest_path, text.replace("\"0.1.0\"", "\"v1\"")).unwrap();
    let policy = PluginSourcePolicy::new(vec![allowed(SHA_ABC)]);
    let error = policy.verify(&path).expect_err("非 semver 版本必须被拒绝");
    assert!(error.contains("semver"), "{error}");
}

#[test]
fn rejects_an_allow_list_identity_mismatch() {
    let path = fixture("identity-mismatch", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let policy = PluginSourcePolicy::new(vec![AllowedPlugin {
        id: "other.plugin".to_string(),
        ..allowed(SHA_ABC)
    }]);
    let error = policy.verify(&path).unwrap_err();
    assert!(error.contains("id"), "{error}");
}

/// 进程级 active policy 会让测试互相污染：拿锁并保证 Drop 时复位为 `None`。
struct ActivePolicyGuard {
    _lock: MutexGuard<'static, ()>,
}

static POLICY_LOCK: Mutex<()> = Mutex::new(());

impl ActivePolicyGuard {
    fn new() -> Self {
        let lock = POLICY_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        set_active_policy(None);
        set_allow_unverified(false);
        Self { _lock: lock }
    }

    fn set(&self, policy: PluginSourcePolicy) {
        set_active_policy(Some(Arc::new(policy)));
    }
}

impl Drop for ActivePolicyGuard {
    fn drop(&mut self) {
        set_active_policy(None);
        set_allow_unverified(false);
    }
}

fn entry_for(path: &Path) -> Entry {
    Entry {
        name: path.to_string_lossy().into_owned(),
        ..Entry::default()
    }
}

/// 预检用例的最小 [`LoaderConfig`]：`strict_sidecars` 打开，让 `plan_entry` 的错误
/// 直接变成装载失败（缺省 `false` 只会把它降级成"这一行没有计划"）。
fn loader_config(path: &Path) -> LoaderConfig {
    LoaderConfig {
        path: path.to_string_lossy().into_owned(),
        patches: Vec::new(),
        plugin_dir: None,
        strict: true,
        strict_cycles: false,
        strict_sidecars: true,
        hmr: None,
        js_evaluator: None,
        interpolation: InterpolationMode::Builtin,
        dispatch_policy: None,
        host_limits: None,
        dependency_plan: Vec::new(),
    }
}

fn assert_no_staged_copy(path: &Path) {
    let dir = path.parent().expect("fixture 有父目录");
    let staged: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".r1.") || name.ends_with(".r1"))
        .collect();
    assert!(staged.is_empty(), "策略拒绝时不得产生暂存副本：{staged:?}");
}

#[test]
fn disk_source_without_policy_denies_by_default() {
    let _guard = ActivePolicyGuard::new();
    assert!(active_policy().is_none());

    let path = fixture("disk-no-policy", b"not a dynamic library");
    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("默认拒绝不得装载"),
        Err(error) => error,
    };
    assert!(error.contains("未配置插件来源策略"), "{error}");
}

#[test]
fn disk_source_with_explicit_development_mode_keeps_the_legacy_load_path() {
    let _guard = ActivePolicyGuard::new();
    set_allow_unverified(true);
    let path = fixture("disk-dev-mode", b"not a dynamic library");
    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("假字节不能装载成功"),
        Err(error) => error,
    };
    assert!(
        !error.contains("manifest") && !error.contains("allow-list"),
        "开发模式不得走来源校验：{error}"
    );
}

#[test]
fn disk_source_with_active_policy_rejects_an_unlisted_plugin_before_staging() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("disk-unlisted", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("未列入 allow-list 的插件必须被拒绝"),
        Err(error) => error,
    };
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "{error}"
    );
    assert_no_staged_copy(&path);
}

#[test]
fn disk_source_with_active_policy_rejects_a_tampered_plugin_before_staging() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("disk-tampered", b"abc");
    // manifest 自述 empty 的 hash，但实际文件是 "abc"。
    write_manifest(&path, SHA_EMPTY, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("artifact 与 manifest hash 不一致必须被拒绝"),
        Err(error) => error,
    };
    assert!(error.contains("SHA256"), "{error}");
    assert_no_staged_copy(&path);
}

#[test]
fn probe_with_active_policy_verifies_before_opening() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("probe-unlisted", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = probe_capabilities_in(&path, &[])
        .expect_err("probe 也必须执行策略")
        .to_string();
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "{error}"
    );
    assert_no_staged_copy(&path);
}

// ------------------------------------------------ O9：局部来源策略（跟随来源对象）

/// O9：`SourceAdmission` 的**诊断字面量**与三条语义一一对应。
///
/// `mode()` 是报告/日志用的口径，不是门禁；但它的取值一旦漂移，"这个来源到底是
/// 谁在说了算"就没人说得清——所以在这里钉住三个字符串。
#[test]
fn source_admission_reports_its_own_mode() {
    let _guard = ActivePolicyGuard::new();
    assert_eq!(SourceAdmission::fallback().mode(), "fallback");
    assert_eq!(
        SourceAdmission::strict(PluginSourcePolicy::new(vec![allowed(SHA_ABC)])).mode(),
        "enforced"
    );
    assert_eq!(SourceAdmission::dev_unverified().mode(), "dev-unverified");

    // 三个构造器的解析结论也是常量（只有 fallback 读全局）。
    assert!(SourceAdmission::dev_unverified()
        .resolve()
        .expect("显式 dev 永远放行")
        .is_none());
    assert!(SourceAdmission::strict(PluginSourcePolicy::default())
        .resolve()
        .expect("显式严格永远有策略")
        .is_some());
    assert!(
        SourceAdmission::fallback().resolve().is_err(),
        "默认拒绝（无策略、无开发模式）必须报错"
    );
}

/// O9-A（**行为红**）：显式严格**永不被**进程级开发标志放宽。
///
/// 场景就是"同一进程里另一个 App 传了 `--allow-unverified`"：全局开发模式打开后，
/// 裸 `DiskPluginSource` 会直接放行到 `LoadLibrary`，于是这个未列入 allow-list 的
/// artifact 得到的是**加载失败**而不是**准入拒绝**——两个错误文本完全不同，
/// 一个说"没登记"，另一个说"打不开"。严格来源必须给出前者，且不留暂存副本。
#[test]
fn explicit_strict_source_ignores_the_global_development_flag() {
    let _guard = ActivePolicyGuard::new();
    // 另一个 App 的便利开关：不能让显式严格来源跟着放宽。
    set_allow_unverified(true);
    let path = fixture("strict-vs-global-dev", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let source = PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]),
    );

    let error = match source.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("显式严格的来源不得装载未列入 allow-list 的 artifact"),
        Err(error) => error,
    };
    assert!(
        error.contains("allow-list"),
        "严格来源必须报准入失败（而不是被全局 dev 放宽后报加载失败）：{error}"
    );
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "拒绝必须发生在打开动态库之前：{error}"
    );
    assert_no_staged_copy(&path);
    assert!(
        allow_unverified(),
        "严格来源只是**忽略**全局标志，不得擅自改写它"
    );
}

/// O9-D（**守卫**，改前改后都绿）：没有包装的来源仍然逐字读全局。
///
/// `SourceAdmission::fallback` 的契约就是"等于今天"：`set_active_policy` 设了策略，
/// 裸 `DiskPluginSource` 照样在暂存之前拒绝未列入的 artifact。
#[test]
fn bare_disk_source_still_reads_the_process_wide_policy() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("bare-global-policy", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("裸源必须沿用进程级策略"),
        Err(error) => error,
    };
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "{error}"
    );
    assert_no_staged_copy(&path);
}

/// O9-E：显式开发**只放行自己**，且**不写**进程级标志。
///
/// 反面判据同样是可诊断的：放行之后走的是旧装载路径（打不开这个假字节），
/// 而不是准入拒绝；同时裸源在同一时刻仍然按默认拒绝报"未配置插件来源策略"。
#[test]
fn explicit_dev_source_does_not_write_the_global_flag() {
    let _guard = ActivePolicyGuard::new();
    assert!(!allow_unverified(), "前提：全局开发模式是关的");
    let path = fixture("explicit-dev-source", b"not a dynamic library");
    let source = PolicyPluginSource::dev_unverified(Arc::new(DiskPluginSource));

    let error = match source.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("假字节不能装载成功"),
        Err(error) => error,
    };
    assert!(
        !error.contains("未配置插件来源策略"),
        "显式 dev 来源不该走到默认拒绝：{error}"
    );
    assert!(
        !error.contains("manifest") && !error.contains("allow-list"),
        "显式 dev 来源不做来源校验：{error}"
    );
    assert!(!allow_unverified(), "显式 dev 来源不得写全局开关");

    // 同一个进程、同一刻：裸源仍按全局语义默认拒绝。
    let bare = match DiskPluginSource.load(&entry_for(&path), &[], 1) {
        Ok(_) => panic!("裸源必须默认拒绝"),
        Err(error) => error,
    };
    assert!(bare.contains("未配置插件来源策略"), "{bare}");
}

/// O9-F：两个来源**同时**载入，各按自己的准入，谁也不碰全局。
///
/// 用两个线程而不是 `tokio::join!`：`PluginSource::load` 是同步方法，单任务里
/// `join!` 只会顺序执行（轮不到 await 点），那样"并发"就只是一句措辞。
///
/// 三方栅栏（两个来源线程 + 主线程）把两次 `load` 钉在**同一个起点**上：顺序执行时，
/// "先写全局 → 调用 → 再复位"的错误实现照样绿（两次调用根本不重叠）；有了栅栏，
/// 那个写全局的窗口会与另一条线程的准入判定重叠，于是能从 strict 侧观察到放宽。
#[test]
fn two_sources_keep_their_own_admission_without_touching_globals() {
    let _guard = ActivePolicyGuard::new();
    let dev_path = fixture("two-sources-dev", b"not a dynamic library");
    let strict_path = fixture("two-sources-strict", b"abc");
    write_manifest(&strict_path, SHA_ABC, "abc123");
    let dev_source: Arc<dyn PluginSource> = Arc::new(PolicyPluginSource::dev_unverified(Arc::new(
        DiskPluginSource,
    )));
    let strict_source: Arc<dyn PluginSource> = Arc::new(PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]),
    ));
    let barrier = Barrier::new(3);

    let (dev_error, strict_error) = std::thread::scope(|scope| {
        let dev = scope.spawn(|| {
            barrier.wait();
            dev_source.load(&entry_for(&dev_path), &[], 1)
        });
        let strict = scope.spawn(|| {
            barrier.wait();
            strict_source.load(&entry_for(&strict_path), &[], 1)
        });
        // 主线程也参与：栅栏不放行，两个来源线程一个都动不了。
        barrier.wait();
        let result = |result: Result<cordis_host::loader::LoadedFile, String>| match result {
            Ok(_) => panic!("两个假 artifact 都不该装载成功"),
            Err(error) => error,
        };
        (
            result(dev.join().expect("dev 线程不得 panic")),
            result(strict.join().expect("strict 线程不得 panic")),
        )
    });

    assert!(
        !dev_error.contains("allow-list") && !dev_error.contains("manifest"),
        "显式 dev 的来源不该做准入校验：{dev_error}"
    );
    assert!(
        !dev_error.contains("未配置插件来源策略"),
        "显式 dev 的来源不该走到默认拒绝（两种拒绝必须能分辨）：{dev_error}"
    );
    assert!(
        strict_error.contains("allow-list"),
        "显式严格的来源必须报准入失败：{strict_error}"
    );
    assert!(
        !strict_error.contains("LoadLibrary") && !strict_error.contains("dlopen"),
        "{strict_error}"
    );
    assert!(
        active_policy().is_none() && !allow_unverified(),
        "两个来源都不得改写进程级准入状态"
    );
}

/// O9-G：**预检**也跟来源对象走。
///
/// 来源被包成 `PolicyPluginSource::strict`：sidecar 合法、artifact 未列入 allow-list，
/// 而全局开发模式开着（另一个 App 的便利开关）。`load_with_source` 的依赖预检必须
/// 先按来源自己的结论拒绝——报的是**准入**原因，而不是 manifest 的问题，也不是
/// "未配置插件来源策略"。这条钉住 `plan_entry_with` 里的 `policy.verify` 分支。
#[tokio::test]
async fn load_with_source_preflights_with_the_sources_own_admission() {
    let _guard = ActivePolicyGuard::new();
    set_allow_unverified(true);
    let path = fixture("preflight-strict", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    let config_path = path.with_file_name("cordis.yml");
    std::fs::write(&config_path, "- id: provider\n  name: plugin.dll\n").unwrap();
    let source: Arc<dyn PluginSource> = Arc::new(PolicyPluginSource::strict(
        Arc::new(DiskPluginSource),
        PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]),
    ));
    let app = App::new();

    let error = load_with_source(app.root.clone(), loader_config(&config_path), source)
        .await
        .expect_err("严格来源的预检必须拒绝未列入 allow-list 的 artifact")
        .to_string();
    assert!(
        error.contains("未通过来源准入"),
        "预检必须报来源准入失败：{error}"
    );
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("sidecar manifest 无效"),
        "准入判定先于 manifest 结论，不该报清单无效：{error}"
    );
    assert!(
        !error.contains("未配置插件来源策略"),
        "显式严格的来源不该走到全局默认拒绝：{error}"
    );
    assert!(
        !error.contains("LoadLibrary") && !error.contains("dlopen"),
        "拒绝必须发生在打开动态库之前：{error}"
    );
    assert_no_staged_copy(&path);
}

/// O9-H：**兼容回落对预检同样成立**。
///
/// 裸 `DiskPluginSource` 的 `plan_entry` 走 `SourceAdmission::fallback`，逐字读进程级
/// 策略：`set_active_policy(Some(未列入))` 之后，预检必须在读 sidecar 结论之前
/// 就按准入拒绝——否则"预检按全局、装载按局部"的裂缝会从这一侧打开。
#[test]
fn bare_disk_source_plan_entry_still_reads_the_process_wide_policy() {
    let guard = ActivePolicyGuard::new();
    let path = fixture("bare-plan-policy", b"abc");
    write_manifest(&path, SHA_ABC, "abc123");
    guard.set(PluginSourcePolicy::new(vec![allowed(SHA_EMPTY)]));

    let error = DiskPluginSource
        .plan_entry(&entry_for(&path), &[])
        .expect_err("裸源的预检必须沿用进程级策略");
    assert!(
        error.contains("未通过来源准入"),
        "预检必须报来源准入失败：{error}"
    );
    assert!(error.contains("allow-list"), "{error}");
    assert!(
        !error.contains("sidecar manifest 无效"),
        "准入判定先于 manifest 结论：{error}"
    );
}
